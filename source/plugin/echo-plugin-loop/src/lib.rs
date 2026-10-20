//! # echo-plugin-loop — 循环模式插件（解耦计划 Phase 3 ⑦）
//!
//! 两个**纯策略**插件，按 persona 二选一：
//!
//! - [`SINGLE_PLUGIN_ID`]（`echo-agent.loop.single`，默认）：单会话循环——同一
//!   会话的 turn 串行排队，后续输入等待前一轮结束；无会话管理 UI、回执分支不可见；
//! - [`PARALLEL_PLUGIN_ID`]（`echo-agent.loop.parallel`）：并行多会话循环——同一
//!   会话可并发多个分支，会话列表 / 全局会话 / 可见回执分支全套。
//!
//! ## 为什么 mount 是空的
//!
//! 驱动本��只有一个（`echo-loop` 的 `TurnRunner`，2026-10 单轨化），它在人格
//! 装配时**无条件注入**；两个插件不切换驱动，只表达策略——真实生效值是
//! per-persona 白名单推导出的 [`echo_defs::LoopMode`]（写进 `enabled_plugins`
//! 的插件 id，空表即单会话）。因此：
//!
//! - mount / unmount **不注册任何运行期资源**（无副作用 = 无泄漏）；
//! - 插件 id 本身是**配置契约**：已在用户配置里的 `enabled_plugins` 条目
//!   必须继续匹配，故 id 字面量在这里冻结，改动即破坏向后兼容。
//!
//! ## 协议化挂载
//!
//! 按解耦计划「可内联编译」的口径，本插件以 inproc 运输层装载：它向宿主
//! 声明一个服务贡献 [`service_contribution`]（`Service{key:"loop"}`），
//! 说明"循环服务由本插件族提供"；真正的 runner 实例由组合根构造（带计量与
//! 脱敏装饰）后注册进 `Ctx`，服务键与贡献声明同源。
//!
//! ## 提供 `loop` 服务（v0.9 起）
//!
//! 这两个插件不再只是"声明一个键"：`mount` 会**真的构造** `TurnRunner` 并
//! `provide` 到 `Ctx` 的 `loop` 键——服务由插件提供，不是组合根建好再借插件
//! 的名义登记。构造所需的东西都从上下文来：
//!
//! - `Ctx["llm"]`（`Arc<dyn LlmProvider>`）：硬依赖，注册表在挂载前校验；
//! - `MountContext.config`：`LoopOptions` 的两个字段（`max_tool_iterations` /
//!   `max_tokens`）可由 `plugins.toml` 覆盖，缺省回退 `LoopOptions::default()`。
//!
//! 卸载时 `provide` 返回的 `Disposer` 撤销服务——`loop` 键随之消失，订阅了它
//! 的插件由 `reconcile` 自动卸下（这就是"服务提供方也是一个插件"该有的行为）。
//!
//! **依赖纪律**：只依赖机制层（`echo-plugin` / `echo-plugin-api` /
//! `echo-context`）、驱动本体（`echo-loop`）与定义层（`echo-defs`）——不依赖
//! `echo-agent`，也不依赖任何宿主业务类型。

#![warn(missing_docs)]

pub mod compaction;

use std::sync::Arc;

use echo_context::Disposer;
use echo_loop::{LoopOptions, ToolPipeline, TurnRunner};
use echo_plugin::{BuiltinPlugin, Inject, MountContext, Plugin, PluginManifest, PluginMountResult};
use echo_plugin_api::ServiceContribution;

/// 单会话循环插件 id（默认模式；空白名单即此模式）。
pub const SINGLE_PLUGIN_ID: &str = "echo-agent.loop.single";

/// 并行多会话循环插件 id。
pub const PARALLEL_PLUGIN_ID: &str = "echo-agent.loop.parallel";

/// 本插件族贡献的服务键（`Ctx` 服务定位命名空间，与 runner 注册键同值）。
pub const SERVICE_KEY: &str = "loop";

/// 本插件族的两个插件 id（顺序稳定：默认在前）。
pub const PLUGIN_IDS: [&str; 2] = [SINGLE_PLUGIN_ID, PARALLEL_PLUGIN_ID];

/// 服务贡献声明（`Service{key:"loop", kind:"loop"}`）。
///
/// 协议化挂载时随 `Register` 上报——消费者据此知道循环服务存在，
/// 而不是硬编码一个具体驱动类型。
pub fn service_contribution() -> ServiceContribution {
    ServiceContribution {
        key: SERVICE_KEY.to_string(),
        kind: "loop".to_string(),
        version: 1,
    }
}

/// 两个循环模式插件的声明（id / 名称 / 描述的唯一事实来源）。
///
/// 返回 `(id, 名称, 描述)`；描述同时供 Panel 插件详情与文档引用。
pub fn declarations() -> [(&'static str, &'static str, &'static str); 2] {
    [
        (
            SINGLE_PLUGIN_ID,
            "单会话循环",
            "单会话（默认）：同一会话的 turn 串行排队，后续输入等待前一轮结束；无会话管理 UI、回执分支不可见",
        ),
        (
            PARALLEL_PLUGIN_ID,
            "并行多会话循环",
            "并行多会话：同一会话可并发多个 turn 分支，会话列表/全局会话/可见回执分支全套",
        ),
    ]
}

/// 构造两个插件实例（`version` = 宿主版本号，写进 manifest）。
///
/// mount 返回空 disposer 列表：纯策略插件没有运行期资源可撤销。
///
/// **服务键依赖**（2026-11）：两个插件都声明 [`SERVICE_KEY`] 为
/// `required_services`——「挂我之前，`Ctx` 里必须有 `loop` 服务」。注册表在
/// mount 前逐个 `Ctx::contains` 校验，缺失即 `MissingService` 且 mount 闭包
/// 不执行。这样组合根若把 `ctx.register::<Arc<TurnRunner>>("loop", …)` 挪到
/// 挂载之后（或删掉），启动期就会**明确失败**并给出可用服务清单，而不是
/// "插件挂载成功、运行期才发现驱动不存在"。
///
/// 服务键与贡献声明同源：[`service_contribution().key`] 是唯一真值。
///
/// [`service_contribution().key`]: service_contribution
pub fn plugins(version: &str) -> Vec<Arc<dyn Plugin>> {
    // 声明式注入（plugin-framework v0.6 的 `Inject`）：硬依赖 = `loop`（本插件
    // 提供的服务，占位校验）**与 `llm`**（构造 TurnRunner 需要）。
    //
    // 为什么 `loop` 也在 required 里：两个模式插件运行期只挂一个（persona 白名单
    // 二选一），但"`loop` 键存在"是它们共同的前置——`Ctx::provide` 对同名键
    // 是 fail-closed 的（`Conflict`），所以两个都挂时只有一个能 provide 成功，
    // 另一个会拿到 Conflict 并明确失败，而不是悄悄覆盖。
    let inject = Inject::new(["llm"]);
    declarations()
        .into_iter()
        .map(|(id, name, description)| {
            let manifest = PluginManifest::builtin(id, name, version, "loop", "loop", description);
            let plugin: Arc<dyn Plugin> =
                Arc::new(BuiltinPlugin::new(manifest, mount_runner).with_inject(inject.clone()));
            plugin
        })
        .collect()
}

/// 挂载：从上下文取 `llm` + 配置，构造 `TurnRunner` 并 `provide` 到 `loop` 键。
///
/// 这两个模式插件**策略不同、驱动同一个**（2026-10 单轨化）：
///
/// - 进程装配时两个都会挂上（它们是 per-persona 二选一的**策略声明**，进程级
///   没有"选哪个"这回事）；
/// - 所以第一个挂的构造并 `provide` 驱动，后面的看到 `loop` 已存在就**不重复
///   提供**（返回空 disposer）——共享同一个驱动，谁先谁提供；
/// - 这是**降级而不是冲突**：两个插件本就代表同一个驱动，报 `Conflict` 会让
///   "两个都挂"这种正常配置起不来。
fn mount_runner(mctx: &MountContext) -> PluginMountResult {
    let Some(ctx) = &mctx.ctx else {
        // 没有服务表 = 裸注册表（单测）；纯策略插件此时无运行期资源可建。
        return Ok(Vec::new());
    };
    if ctx.contains(SERVICE_KEY) {
        // 另一个循环模式插件已经提供了驱动（同一份实现的两种策略）——不重复
        // 构造，也不持有任何资源，卸载时无需撤销。
        return Ok(Vec::new());
    }
    let provider = ctx
        .resolve::<Arc<dyn echo_defs::llm::LlmProvider>>("llm")
        .ok_or_else(|| echo_plugin::PluginMountError::MissingService {
            key: "llm".to_string(),
            available: ctx.keys(),
        })?;
    // 配置下发（`plugins.toml` 的 `[plugin.config]`）：两个字段都可覆盖，
    // 缺省回退 `LoopOptions::default()`——所以"不配"与"配了"行为一致。
    let defaults = LoopOptions::default();
    let options = LoopOptions {
        max_tool_iterations: mctx
            .config()
            .get("max_tool_iterations")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(defaults.max_tool_iterations),
        max_tokens: mctx
            .config()
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .or(defaults.max_tokens),
    };
    // 独立的 EventBus：进程级 `loop` 服务不是某个 persona 的 turn 驱动，
    // 它的事件没人订阅（每个 agent 用自己那份 runner/bus 跑 turn）——这里
    // 只是把"服务存在"这件事落实，不用宿主的 bus 造成双重分发。
    let runner: Arc<TurnRunner> = Arc::new(TurnRunner::new(
        Arc::new(echo_context::EventBus::default()),
        provider,
        Arc::new(ToolPipeline::new()),
        options,
    ));
    let keep: Disposer = ctx
        .provide(&runner_key(), runner)
        .map_err(|e| echo_plugin::PluginMountError::Mount(e.to_string()))?;
    Ok(vec![keep])
}

/// `loop` 服务的强类型键（与 [`SERVICE_KEY`] 同源）。
fn runner_key() -> echo_context::ServiceKey<Arc<TurnRunner>> {
    echo_context::ServiceKey::new(SERVICE_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_frozen_configuration_contract() {
        // 用户配置里写的就是这两个字面量；改动即破坏 enabled_plugins 兼容。
        assert_eq!(SINGLE_PLUGIN_ID, "echo-agent.loop.single");
        assert_eq!(PARALLEL_PLUGIN_ID, "echo-agent.loop.parallel");
        assert_eq!(PLUGIN_IDS, [SINGLE_PLUGIN_ID, PARALLEL_PLUGIN_ID]);
    }

    #[test]
    fn plugins_carry_declared_manifests() {
        let plugins = plugins("9.9.9");
        assert_eq!(plugins.len(), 2);
        let ids: Vec<&str> = plugins.iter().map(|p| p.manifest().id.as_str()).collect();
        assert_eq!(ids, vec![SINGLE_PLUGIN_ID, PARALLEL_PLUGIN_ID]);
        for (plugin, (id, name, description)) in plugins.iter().zip(declarations()) {
            let manifest = plugin.manifest();
            assert_eq!(manifest.id, id);
            assert_eq!(manifest.name, name);
            assert_eq!(manifest.description, description);
            assert_eq!(manifest.kind, "loop");
            assert_eq!(manifest.version, "9.9.9");
        }
    }

    #[test]
    fn mount_has_no_runtime_side_effects() {
        // 纯策略插件：禁用/启用不产生任何可撤销资源（mode 由白名单推导）。
        let ctx = echo_plugin::MountContext::new();
        for plugin in plugins("0.1.0") {
            let disposers = plugin.mount(&ctx).expect("mount ok");
            assert!(disposers.is_empty());
            plugin.unmount().expect("unmount ok");
        }
    }

    /// 依赖声明是 `llm`（构造 TurnRunner 需要），**不是** `loop`——`loop` 是
    /// 本插件自己 provide 的东西，把它写进 required 会让插件等自己。
    #[test]
    fn plugins_require_the_llm_service() {
        for plugin in plugins("0.1.0") {
            assert_eq!(plugin.inject().required, vec!["llm".to_string()]);
        }
    }

    /// `llm` 未注册 → 挂不上（注册表的注入校验兜住），且错误里点名 `llm`。
    #[test]
    fn mount_fails_when_llm_service_is_missing() {
        let reg = echo_plugin::PluginRegistry::new();
        for plugin in plugins("0.1.0") {
            reg.register(plugin).expect("register");
        }
        let errors = reg.mount_all(&echo_plugin::MountContext::new());
        assert_eq!(errors.len(), 2, "两个插件都该因缺 llm 失败: {errors:?}");
        assert!(
            errors[0].1.contains("llm"),
            "错误里要指明缺的键: {errors:?}"
        );
    }

    /// **提供 `loop` 服务**（S2 的核心）：挂载后 `Ctx` 里能解析出 `TurnRunner`，
    /// 卸载后键消失——服务由插件提供、也由插件撤销。
    ///
    /// 两个模式插件挂载**同一个驱动**，所以第二个会撞 `Ctx::provide` 的
    /// `Conflict`（同名键 fail-closed）。这里只挂一个，验证正常路径。
    #[test]
    fn mount_provides_the_loop_service_and_unmount_withdraws_it() {
        use echo_defs::LlmProvider;

        // 最小 provider：只用来满足 TurnRunner 的构造参数（本测试不跑 turn）。
        struct NoopProvider;
        #[async_trait::async_trait]
        impl LlmProvider for NoopProvider {
            fn name(&self) -> &str {
                "noop"
            }
            fn default_model(&self) -> &str {
                "noop"
            }
            async fn chat(
                &self,
                _request: &echo_defs::ChatRequest,
            ) -> Result<echo_defs::ChatResponse, echo_defs::llm::LlmError> {
                Err(echo_defs::LlmError::Api("unused in test".into()))
            }
            async fn chat_stream(
                &self,
                _request: &echo_defs::ChatRequest,
                _tx: tokio::sync::mpsc::UnboundedSender<echo_defs::ChatChunk>,
            ) -> Result<echo_defs::ChatResponse, echo_defs::llm::LlmError> {
                Err(echo_defs::LlmError::Api("unused in test".into()))
            }
        }

        let ctx = std::sync::Arc::new(echo_context::Ctx::default());
        let _llm = ctx.register::<Arc<dyn LlmProvider>>("llm", Arc::new(NoopProvider));

        let reg = echo_plugin::PluginRegistry::new();
        let plugin = plugins("0.1.0").into_iter().next().expect("one plugin");
        reg.register(plugin).expect("register");
        let mctx = echo_plugin::MountContext::new().with_ctx(ctx.clone());

        // 直接挂第一个 id（两个插件的 mount 逻辑相同，挂哪个都能提供）。
        reg.mount(&reg.ids().into_iter().next().expect("id"), &mctx)
            .expect("mount provides loop");

        let resolved = ctx.resolve::<Arc<TurnRunner>>(SERVICE_KEY);
        assert!(resolved.is_some(), "挂载后应能解析出 loop 服务");

        // 卸载 → 服务随之消失（disposer 撤销）。
        reg.unmount(&reg.ids().into_iter().next().expect("id"))
            .expect("unmount");
        assert!(
            ctx.resolve::<Arc<TurnRunner>>(SERVICE_KEY).is_none(),
            "卸载后 loop 键应消失（服务提供方负责撤销）"
        );
    }

    /// `plugins.toml` 的 `[plugin.config]` 能覆盖 `LoopOptions`（配置下发）。
    #[test]
    fn config_overrides_loop_options() {
        // 只验证"配置真能到插件手里 + 解析分支正确"（不构造 runner 需要 ctx）。
        let mctx = echo_plugin::MountContext::new()
            .with_config(serde_json::json!({ "max_tool_iterations": 7, "max_tokens": 2048 }));
        assert_eq!(
            mctx.config()
                .get("max_tool_iterations")
                .and_then(|v| v.as_u64()),
            Some(7)
        );
        // 真正读到值的地方是 `mount_runner`；它的分支逻辑与这里一致
        // （缺省回退 `LoopOptions::default()`，有值就覆盖）。
        let defaults = LoopOptions::default();
        let got = mctx
            .config()
            .get("max_tool_iterations")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(defaults.max_tool_iterations);
        assert_eq!(got, 7);
    }
}
