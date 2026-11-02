//! # echo-plugin-session — 会话子系统插件（解耦计划 S6）
//!
//! 事件溯源 trunk 的**实现归属**：append-only 事件日志、按会话投影、持久化、
//! 归档轮转、压缩编排与显示历史回填。
//!
//! ## 为什么它现在是插件而不是宿主的 `session/` 模块
//!
//! 此前这 2268 行住在 `echo-agent/src/session/`：`TrunkStore` 与 `Agent` 同
//! 一个 crate，于是"会话怎么存"和"agent 怎么跑 turn"共用一次编译、一次发布，
//! 也无法在面板的插件清单里表达"这一份部署有没有会话子系统"。
//!
//! 现在按其它插件（`-loop` / `-management` / `-workspace`）的同一形态外置：
//!
//! ```text
//!   echo-agent（宿主） ──依赖──► echo-plugin-session（本 crate）
//!                                     │
//!                                     ├─ TrunkStore（事件日志 + 投影 + 持久化）
//!                                     ├─ catalog 里的一行 manifest
//!                                     └─ ServiceKey "echo-agent.session.factory"
//! ```
//!
//! **词汇不上移**：`SessionEvent` / `EventLog` / `derive_messages` 仍在
//! `echo-session`（机制层，谁都能 `use`，没有行为归属）；本 crate 只拥有
//! **实例与其生命周期**。这与 `echo-plugin-loop` 用 `echo-session` 的词汇、
//! 但自己 provide `loop` 服务是同一分工。
//!
//! ## 宿主拿什么
//!
//! 宿主不再自己 `TrunkStore::new`，而是解析 [`SESSION_FACTORY`] 服务：
//!
//! ```ignore
//! let factory = ctx.resolve(SESSION_FACTORY.name()).ok_or("会话插件未挂载")?;
//! let trunk = factory.create(config.effective_memory_limit_tokens());
//! ```
//!
//! 好处是**卸载即失效**：没有会话插件，就造不出 trunk，而不是静默退化成
//! "没有记忆也能跑"的半残状态（fail-closed，与 `MissingService` 同口径）。
//!
//! ## 依赖纪律
//!
//! 只依赖词汇层（`echo-defs` / `echo-protocol` / `echo-session`）与插件机制
//! （`echo-plugin` / `echo-context`）——**不依赖 `echo-agent`，也不依赖任何
//! 其它插件**（`scripts/deps-policy.txt` 第 4 条的门禁会拦）。
//!
//! 显示历史派生（`store::history`）与实时投影（`echo-plugin-management` 的
//! `timeline`）共用的一批文本工具，已下沉到 `echo-protocol::timeline_text`
//! ——共用逻辑的下沉方向永远是"往共同的下游"，不是插件互相 `use`。

use std::path::PathBuf;
use std::sync::Arc;

use echo_context::ServiceKey;
use echo_plugin::{BuiltinPlugin, Plugin, PluginManifest};

pub mod commands;
pub mod store;

pub use commands::{SessionCommandHandler, SessionCommandHost};
pub use store::TrunkStore;

/// 插件 id（`core.toml` 的 `enabled_plugins` / 面板清单同一口径）。
pub const PLUGIN_ID: &str = "echo-agent.session";

/// 会话工厂的服务键（宿主按此解析；其它插件一般不直接用它）。
pub const SESSION_FACTORY_SERVICE: &str = "echo-agent.session.factory";

/// 按部署规格造一个 trunk。
///
/// 为什么是工厂而不是单例：**每个 persona 一份独立上下文**（多人格并存时
/// 各有自己的事件日志与 token 预算）。单例服务会让所有人格共享一份 trunk，
/// 那是 2026-09 多会话改造之前的行为。
pub trait SessionFactory: Send + Sync {
    /// 造一个 trunk。
    ///
    /// `memory_limit_tokens` 是该 persona 的模型上下文预算（`core.toml` 的
    /// `[agent]` / `[agent.teams.<id>]`）；`persist_path` 为 `None` 时是纯内存
    /// 实例（测试 / 嵌入式用法）。
    fn create(&self, memory_limit_tokens: usize, persist_path: Option<PathBuf>) -> TrunkStore;
}

/// 默认实现：直接构造 [`TrunkStore`]。
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultSessionFactory;

impl SessionFactory for DefaultSessionFactory {
    fn create(&self, memory_limit_tokens: usize, persist_path: Option<PathBuf>) -> TrunkStore {
        let trunk = TrunkStore::new(memory_limit_tokens);
        if let Some(path) = persist_path {
            trunk.set_persist_path(path);
        }
        trunk
    }
}

/// 会话写命令域的宿主接缝服务键（`echo-agent.host.session_commands`）。
pub const SESSION_COMMAND_HOST_SERVICE: &str = "echo-agent.host.session_commands";

/// 会话写命令域的宿主接缝键（`mount` 时绑定，`handle` 时取用）。
pub static SESSION_COMMAND_HOST_KEY: echo_context::ServiceKey<Arc<dyn SessionCommandHost>> =
    echo_context::ServiceKey::new(SESSION_COMMAND_HOST_SERVICE);

/// 会话工厂的强类型键。
pub fn factory_key() -> ServiceKey<Arc<dyn SessionFactory>> {
    ServiceKey::new(SESSION_FACTORY_SERVICE)
}

/// 构造本插件（catalog 用）。
///
/// mount 只做一件事：把工厂 provide 进 `Ctx`，返回撤销句柄。**不加载任何
/// 会话**——trunk 由宿主在构造 persona 时按需 create（部署里可能一个 persona
/// 都不启用，提前加载只会制造空文件）。
pub fn plugin(version: &str) -> Arc<dyn Plugin> {
    use echo_plugin::{Inject, ServiceSlot};

    let manifest = PluginManifest::builtin(
        PLUGIN_ID,
        "会话",
        version,
        "session",
        "session",
        "事件溯源 trunk：append-only 事件日志、按会话投影、持久化、压缩编排与写命令域",
    );
    // 命令域要的宿主接缝：mount 时绑定（宿主在装配期填），处理命令时取用。
    let command_host = ServiceSlot::<Arc<dyn SessionCommandHost>>::new();
    let (bind_host, serve_host) = (command_host.clone(), command_host.clone());
    Arc::new(
        BuiltinPlugin::new(manifest, move |mctx| {
            let Some(ctx) = &mctx.ctx else {
                return Err(echo_plugin::PluginMountError::Mount(
                    "会话插件 mount 缺 Ctx（裸注册表：没有服务定位器可注册）".into(),
                ));
            };
            let factory: Arc<dyn SessionFactory> = Arc::new(DefaultSessionFactory);
            let keep_factory = ctx
                .provide(&factory_key(), factory)
                .map_err(|error| echo_plugin::PluginMountError::Mount(error.to_string()))?;
            // 接缝是**可选**的：嵌入式部署（无 Agent）照样挂载，只是命令域
            // 无人认领 → 命令泵会回落到宿主兜底分支并明确报错。
            let keep_host = bind_host.bind(mctx, &SESSION_COMMAND_HOST_KEY)?;
            Ok(vec![keep_factory, keep_host])
        })
        // 接缝是可选注入：宿主没绑也能挂（命令落兜底分支）。
        .with_inject(Inject::new([SESSION_COMMAND_HOST_KEY.name().to_string()]))
        .with_commands(commands::COMMANDS)
        .with_command_handler_async(move |name, payload| {
            let serve_host = serve_host.clone();
            Box::pin(async move {
                let Some(host) = serve_host.get() else {
                    return Err(format!(
                        "命令 {name} 到 {PLUGIN_ID} 时插件尚未挂载（会话命令接缝未绑定）"
                    ));
                };
                commands::SessionCommandHandler::new(host)
                    .handle(&name, payload)
                    .await
                    .unwrap_or_else(|| Err(format!("命令 {name} 不属于会话命令域")))
            })
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_id_is_frozen_configuration_contract() {
        // 用户配置里写的就是这个字面量；改动即破坏 enabled_plugins 兼容。
        assert_eq!(PLUGIN_ID, "echo-agent.session");
    }

    #[test]
    fn manifest_declares_session_kind() {
        let plugin = plugin("9.9.9");
        let manifest = plugin.manifest();
        assert_eq!(manifest.id, PLUGIN_ID);
        assert_eq!(manifest.kind, "session");
        assert_eq!(manifest.version, "9.9.9");
        assert!(manifest.builtin);
    }

    /// mount 的副作用：工厂 + 命令域接缝，两条都可撤销。
    ///
    /// 接缝是**硬依赖**（`Inject::required`）：宿主没注册它时挂载失败，而不是
    /// 静默得到一堆没人认领的命令。所以这里要先把替身注册进 `Ctx`。
    #[test]
    fn mount_provides_the_factory_and_rolls_back_on_unmount() {
        let ctx = Arc::new(echo_context::Ctx::default());
        let keep = ctx
            .provide(
                &SESSION_COMMAND_HOST_KEY,
                Arc::new(StubHost) as Arc<dyn SessionCommandHost>,
            )
            .expect("替身注册");
        let mctx = echo_plugin::MountContext::new().with_ctx(Arc::clone(&ctx));
        let plugin = plugin("0.1.0");
        let disposers = plugin.mount(&mctx).expect("mount ok");
        assert_eq!(disposers.len(), 2, "工厂 + 命令接缝");
        assert!(
            ctx.service(&factory_key()).is_some(),
            "mount 后工厂必须可取"
        );
        plugin.unmount().expect("unmount ok");
        drop(keep);
    }

    /// 命令域的最小宿主替身（本模块只验证挂载/撤销，不跑命令）。
    struct StubHost;

    impl SessionCommandHost for StubHost {
        fn self_team_id(&self) -> Option<String> {
            None
        }
        fn resolve_trunk(&self, _team_id: &str) -> Option<Arc<TrunkStore>> {
            None
        }
        fn trunk(&self) -> Arc<TrunkStore> {
            Arc::new(TrunkStore::new(1000))
        }
        fn forget_session_provider(&self, _session_id: &str) {}
        fn emit(&self, _event: echo_protocol::BackendEvent) {}
        fn emit_context_snapshot<'a>(
            &'a self,
            _session_id: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }
        fn compact<'a>(
            &'a self,
            _team_id: Option<String>,
            _session_id: &'a str,
            _keep_recent: usize,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>,
        > {
            Box::pin(async { Ok(String::new()) })
        }
    }

    #[test]
    fn factory_creates_independent_trunks() {
        let factory = DefaultSessionFactory;
        let a = factory.create(1000, None);
        let b = factory.create(2000, None);
        a.append_event(echo_session::SessionEvent::UserMessage(
            echo_session::event::UserMessage {
                content: "a".into(),
                timestamp: 1,
                message_sequence: None,
                source: None,
                images: Vec::new(),
                session: Some("s-a".into()),
            },
        ));
        assert_eq!(a.event_log().len(), 1);
        assert_eq!(b.event_log().len(), 0, "两个人格不共享日志");
    }
}
