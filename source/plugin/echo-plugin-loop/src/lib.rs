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
//! **依赖纪律**：只依赖机制层（`echo-plugin` / `echo-plugin-api`）与定义层
//! （`echo-defs`）——不依赖 `echo-agent`，也不依赖具体驱动实现。

#![warn(missing_docs)]

use std::sync::Arc;

use echo_plugin::{BuiltinPlugin, Inject, Plugin, PluginManifest, PluginMountResult};
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
    // 声明式注入（plugin-framework v0.6 的 `Inject`）：硬依赖 = 服务键，
    // 可选软依赖留给将来（如计量/脱敏装饰器就绪时才包一层）。
    let inject = Inject::new([service_contribution().key]);
    declarations()
        .into_iter()
        .map(|(id, name, description)| {
            let manifest = PluginManifest::builtin(id, name, version, "loop", "loop", description);
            let plugin: Arc<dyn Plugin> = Arc::new(
                BuiltinPlugin::new(manifest, |_ctx| -> PluginMountResult { Ok(Vec::new()) })
                    .with_inject(inject.clone()),
            );
            plugin
        })
        .collect()
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

    /// 服务键依赖**从贡献声明派生**（单一真值）：改 `service_contribution`
    /// 的服务键，`required_services` 自动跟着变——不会出现"声明说 loop、
    /// 挂载校验说别的键"这种漂移。
    #[test]
    fn plugins_require_the_declared_service_key() {
        for plugin in plugins("0.1.0") {
            assert_eq!(plugin.required_services(), vec![SERVICE_KEY.to_string()]);
        }
    }

    /// 服务未注册时**挂不上**：这是组合根启动顺序的真实门禁。
    ///
    /// 用 `PluginRegistry` 走一遍（而不是直接 `plugin.mount`）——校验在
    /// 注册表的 mount 里，直接调 trait 方法就绕过它了。
    #[test]
    fn mount_fails_when_loop_service_is_missing() {
        let reg = echo_plugin::PluginRegistry::new();
        for plugin in plugins("0.1.0") {
            reg.register(plugin).expect("register");
        }
        let errors = reg.mount_all(&echo_plugin::MountContext::new());
        assert_eq!(errors.len(), 2, "两个插件都该因缺服务失败: {errors:?}");
        assert!(
            errors[0].1.contains("loop"),
            "错误里要指明缺的键: {errors:?}"
        );

        // 服务就绪后同一批插件挂载成功（幂等重试）。
        let ctx = std::sync::Arc::new(echo_context::Ctx::default());
        let _keep = ctx.register::<u8>(SERVICE_KEY, 1u8);
        let errors = reg.mount_all(&echo_plugin::MountContext::new().with_ctx(ctx));
        assert!(errors.is_empty(), "服务就绪后不该再失败: {errors:?}");
    }

    #[test]
    fn service_contribution_matches_runner_registration_key() {
        let c = service_contribution();
        assert_eq!(c.key, SERVICE_KEY);
        assert_eq!(c.key, "loop");
        assert_eq!(c.kind, "loop");
        assert_eq!(c.version, 1);
    }
}
