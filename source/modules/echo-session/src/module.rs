//! # echo-session — 模块外壳（`echo-agent.kernel.session`）
//!
//! ## 为什么一个库也要是模块
//!
//! 2026-11：**取消「中间层 / 模块」这个二分**。此前 `source/` 下的 crate 被
//! 分成两类——「中间层」（`agent-module-tools` 等，只是库）与「模块」（`echo-module-*`，
//! 有 manifest）。这个区分只存在于人的脑子里：两者都是编译进同一份二进制的
//! crate，都靠 `Ctx` 服务键协作，唯一差别是后者多一个 manifest。
//!
//! 现在统一：**每个 crate 都以模块形态存在**。`kind = "kernel"` 的模块提供的是
//! 其它模块的**运行地基**（工具注册表、事件日志、线协议），因此 `mount` 是
//! **纯身份声明**——没有运行期资源需要申请或撤销，`EventLog` 由 `agent-module-session` 的 `TrunkStore` 持有，不在此申请。
//!
//! ## 与 `kind = "tool"` / `"provider"` 等能力模块的差别
//!
//! | | kernel | 能力模块 |
//! |---|---|---|
//! | 能否 `mount` / `unmount` | ✅（只是没有副作用的空动作） | ✅（有真实资源） |
//! | 能否在 `modules.toml` 里 `enabled = false` | ❌ **启动失败**（见下） | ✅ 注册但不挂载 |
//! | 能否被 persona 白名单关闭 | ❌（`KIND_SCOPE` → Process） | 视 kind 而定 |
//! | 出现在 Panel 模块清单 | ✅ | ✅ |
//!
//! **为什么禁不掉**：事件枚举就是磁盘格式。禁用 = 读不出已有会话文件（用户的全部历史）。
//!
//! 判据是「禁用之后系统还能不能起来」，不是「想不想让它可配」。写
//! `enabled = false` 时会**明确失败并指出这一条**——而不是静默跳过、让运维
//! 以为关掉了一个可有可无的东西（那种"假动作"比不给开关更糟）。

use std::sync::Arc;

use echo_module::{BuiltinModule, Module, ModuleManifest};

/// 模块 id（`modules.toml` 与 Panel 用它寻址）。
pub const MODULE_ID: &str = "echo-agent.kernel.session";

/// 模块类别：**运行地基**。`modules/scope.rs` 的 `KIND_SCOPE` 把它登记为
/// `Process`（人格白名单碰不到它）。
pub const KIND: &str = "kernel";

/// 模块清单。
pub const MANIFEST_TOML: &str = include_str!("../module.toml");

pub fn manifest(version: &str) -> ModuleManifest {
    ModuleManifest::from_toml(MANIFEST_TOML, version)
        .expect("事件溯源模块的 module.toml 解析失败")
}

/// 构造模块：**纯身份声明**（`mount` 返回空 disposer）。
///
/// 不在这里 `provide` 服务的原因：`EventLog` 的实例有**归属**（每个 persona 一份 trunk，由 `agent-module-session` 管理）。在这里 provide 会造出一个"全局日志"的假象，与实际的多 trunk 模型矛盾。
pub fn module(version: &str) -> Arc<dyn Module> {
    Arc::new(BuiltinModule::new(manifest(version), |_ctx| Ok(Vec::new())))
}

/// 单模块族入口（与 `agent_module_loop::modules` / `agent_module_sanitize::modules`
/// 同形，供组合根统一循环）。
pub fn modules(version: &str) -> Vec<Arc<dyn Module>> {
    vec![module(version)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_and_kind_are_frozen_configuration_contract() {
        // `modules.toml` 与 Panel 读的就是这两个字面量。
        assert_eq!(MODULE_ID, "echo-agent.kernel.session");
        assert_eq!(KIND, "kernel");
    }

    #[test]
    fn manifest_is_a_kernel_module() {
        let manifest = manifest("0.0.0-test");
        assert_eq!(manifest.id, MODULE_ID);
        assert_eq!(manifest.kind, "kernel");
        // `builtin = true`：它编译在本二进制里，不是磁盘发现的。
        assert!(manifest.builtin);
    }

    #[test]
    fn mount_is_a_pure_identity_declaration() {
        let module = module("0.0.0-test");
        let ctx = echo_module::MountContext::default();
        let disposers = module.mount(&ctx).expect("mount 不得失败");
        assert!(disposers.is_empty(), "kernel 模块无运行期资源");
    }
}
