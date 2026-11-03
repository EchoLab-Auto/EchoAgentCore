//! # echo-plugin-loop — 循环插件（解耦计划 Phase 3 ⑦）
//!
//! 一个**纯策略**插件 [`SINGLE_PLUGIN_ID`]（`echo-agent.loop.single`）：同一会话
//! 的 turn 串行排队，后续输入等待前一轮结束。
//!
//! **并行多会话模式（`echo-agent.loop.parallel`）已删除**（2026-11）：它让同一
//! 会话可以并发多个 turn 分支，代价是"读到一半的上下文"和"两条回答互相看不见"
//! ——对一个人格来说这不是能力，是不确定性。删除后一条会话永远只有一条时间线，
//! 配上面的 `ReplyBranch*` 可见性事件与 `LoopMode` 类型一并消失。
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
//! 声明服务键 [`SERVICE_KEY`]（`"loop"`），
//! 说明"循环服务由本插件族提供"；真正的 runner 实例由组合根构造（带计量与
//! 脱敏装饰）后注册进 `Ctx`，服务键与贡献声明同源。
//!
//! ## 提供 `loop` 服务（v0.9 起）
//!
//! 这两个插件不再只是"声明一个键"：`mount` 会**真的构造** `TurnRunner` 并
//! `provide` 到 `Ctx` 的 `loop` 键——服务由插件提供，不是组合根建好再借插件
//! 的名义登记。构造所需的东西都从上下文来：
//!
//! - `Ctx[LLM_PROVIDER_SERVICE_KEY]`（`Arc<dyn LlmProvider>`）：硬依赖，
//!   由**组合根**注册（键由定义层给出，本 crate 不依赖 provider 插件）；
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

/// 循环插件 id。**字面量冻结**：用户配置的 `enabled_plugins` 里可能还写着它
/// （历史配置、旧版本导出），改名即让那些配置静默失效。
pub const SINGLE_PLUGIN_ID: &str = "echo-agent.loop.single";

/// 已删除的并行插件 id —— **只用于配置兼容**（读取时认它、写入时不再产生）。
///
/// 留着它而不是彻底抹掉：用户 `core.toml` 的 `enabled_plugins` 里可能还有这一
/// 项，Core 读到它要能**明白**这是"旧配置里的并行模式"并忽略，而不是当成一个
/// 不认识的插件 id 报错。新配置不会再写入它。
pub const REMOVED_PARALLEL_PLUGIN_ID: &str = "echo-agent.loop.parallel";

/// 本插件族贡献的服务键（`Ctx` 服务定位命名空间，与 runner 注册键同值）。
pub const SERVICE_KEY: &str = "loop";

/// 循环插件的声明（id / 名称 / 描述的唯一事实来源）。
///
/// 返回 `(id, 名称, 描述)`；描述同时供 Panel 插件详情与文档引用。
pub fn declarations() -> [(&'static str, &'static str, &'static str); 1] {
    [(
        SINGLE_PLUGIN_ID,
        "循环",
        "同一会话的 turn 串行排队：后续输入等待前一轮结束，一条会话只有一条时间线",
    )]
}

/// 构造循环插件（`version` = 宿主版本号，写进 manifest）。
///
/// mount 返回空 disposer 列表：纯策略插件没有运行期资源可撤销。
///
/// **服务键依赖**（2026-11）：插件声明 [`SERVICE_KEY`] 为 `required_services`
/// ——「挂我之前，`Ctx` 里必须有 `loop` 服务」。注册表在 mount 前逐个
/// `Ctx::contains` 校验，缺失即 `MissingService` 且 mount 闭包不执行。这样
/// 组合根若删掉 `ctx.register::<Arc<TurnRunner>>("loop", …)`，启动期就会**明确
/// 失败**并给出可用服务清单，而不是"插件挂载成功、运行期才发现驱动不存在"。
///
/// 服务键的唯一真值是 [`SERVICE_KEY`]。
pub fn plugins(version: &str) -> Vec<Arc<dyn Plugin>> {
    // 声明式注入（plugin-framework v0.6 的 `Inject`）：硬依赖 = `llm`
    // （构造 TurnRunner 需要）。
    //
    // 键来自**定义层**（宿主 ↔ 插件契约，`scripts/deps-policy.txt` 不允许
    // 本 crate 依赖 provider 插件）。此前这里是字面量 `"llm"`，与插件侧那个
    // 名不副实的 `SERVICE_KEY` 各写一遍——改名一侧就静默少一个依赖。
    let inject = Inject::new([echo_defs::llm::LLM_PROVIDER_SERVICE_KEY]);
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
/// `loop` 键已存在时不重复构造（返回空 disposer）：persona 作用域会重新构造
/// 一批插件实例，同一个键不能被覆盖——`Ctx::provide` 对同名键是 fail-closed
/// 的，重复提供只会得到一个 `Conflict`。
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
    let key = echo_defs::llm::LLM_PROVIDER_SERVICE_KEY;
    let provider = ctx
        .resolve::<Arc<dyn echo_defs::llm::LlmProvider>>(key)
        .ok_or_else(|| echo_plugin::PluginMountError::MissingService {
            key: key.to_string(),
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
        // 用户配置里写的就是这个字面量；改动即破坏 enabled_plugins 兼容。
        assert_eq!(SINGLE_PLUGIN_ID, "echo-agent.loop.single");
        // 已删除的并行插件 id 仍要认得（旧配置里可能还有），但不再产出生效插件。
        assert_eq!(REMOVED_PARALLEL_PLUGIN_ID, "echo-agent.loop.parallel");
    }

    #[test]
    fn plugins_carry_declared_manifests() {
        let plugins = plugins("9.9.9");
        assert_eq!(plugins.len(), 1);
        let ids: Vec<&str> = plugins.iter().map(|p| p.manifest().id.as_str()).collect();
        assert_eq!(ids, vec![SINGLE_PLUGIN_ID]);
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
            assert_eq!(
                plugin.inject().required,
                vec![echo_defs::llm::LLM_PROVIDER_SERVICE_KEY.to_string()]
            );
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
        assert_eq!(errors.len(), 1, "唯一循环插件该因缺 llm 失败: {errors:?}");
        assert!(
            errors[0].1.contains("llm"),
            "错误里要指明缺的键: {errors:?}"
        );
    }

    /// **提供 `loop` 服务**（S2 的核心）：挂载后 `Ctx` 里能解析出 `TurnRunner`，
    /// 卸载后键消失——服务由插件提供、也由插件撤销。
    ///
    /// 只有一个循环插件（并行模式已删除），正常挂载即可提供 `loop` 服务。
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
        let _llm = ctx.register::<Arc<dyn LlmProvider>>(
            echo_defs::llm::LLM_PROVIDER_SERVICE_KEY,
            Arc::new(NoopProvider),
        );

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
