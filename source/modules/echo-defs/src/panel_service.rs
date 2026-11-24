//! 面板服务接缝：模块把自己的能力登记成**有名字的服务**，面板按服务名请求。
//!
//! ## 它解决什么
//!
//! 面板要读的数据（会话列表、模块自己维护的集合…）有两个来源可选：
//!
//! 1. **广播**：宿主在状态变化时发事件，面板累积。问题是没有"现在给我一份
//!    完整快照"的入口——面板连上来时只能靠一条条事件拼，拼不出来就问不到。
//!    2026-11 的现场：会话列表靠 `SessionUpdated` 累积，进程重启后磁盘上有
//!    会话、但没有任何变更事件，面板侧边栏空着直到用户手动动一下工作区。
//! 2. **按模块 id 点方法**（`InvokeModuleOp`）：能拿到，但面板必须硬编码
//!    `module` id 与方法名——模块改名、或同一件事换个模块实现，面板就断。
//!
//! 本接缝提供第三种：**按服务名请求**。名字是稳定的语义名（`echo-agent.sessions`
//! 而不是 `agent-module-session`），面板问"谁会列会话"，而不是"哪个 crate
//! 的哪个方法"。提供方在自己的 `mount` 里登记，归属跟着挂载状态走——模块被
//! 禁用，服务一起消失（与 [`crate::skill::SkillRegistrar`] 同一套归属逻辑）。
//!
//! ## 三层，方向各不相同
//!
//! | 角色 | 谁 | 时机 |
//! |---|---|---|
//! | 注册表（提供方） | Core 组合根 `provide` | 任何模块 mount **之前** |
//! | 登记（模块 → 注册表） | 各模块在自己的 `mount` 里 [`PanelServiceRegistrar::register`] | 挂载时；卸载时撤销 |
//! | 请求（面板 → 注册表） | 线协议命令 → 命令泵 → [`PanelServiceInvoker::invoke`] | 每次用户动作 |
//!
//! 注册面与分发面是**两个 trait**：模块拿到的句柄只能登记，不能遍历别人的
//! 服务（否则"服务"就退化成一张全局函数表，与直接互相依赖没有区别）；命令泵
//! 只需要分发这一个能力。
//!
//! ## 为什么载荷是 `serde_json::Value`
//!
//! 每个服务的入参出参都不一样，而定义层不该认识任何模块的数据类型（否则
//! `echo-defs` 会依赖所有模块）。形状由**服务自己**约定并写进 `methods()` 的
//! 文档：注册表只做路由，不解释载荷。
//!
//! 代价是调用方与提供方要各自对齐形状——所以面板侧的等待销账按 `request_id`
//! 配对（协议内建），而不是靠"事件名对应命令名"的映射表：一个服务有多个
//! 方法时，后者根本表达不了。

use std::sync::Arc;

use async_trait::async_trait;
use echo_context::ServiceKey;

use crate::disposer::Disposer;

/// 一个对面板开放的服务。
///
/// 实现方是**模块**（不是宿主）：模块认识自己的数据，注册表只路由。
#[async_trait]
pub trait PanelService: Send + Sync {
    /// 服务名（语义名，稳定）。
    ///
    /// 惯例：`echo-agent.<名词复数>`，如 `echo-agent.sessions`。**不要**把方法
    /// 名拼进服务名（`echo-agent.session.list` 会让"服务"与"方法"两级混在
    /// 一起，加第二个方法时无路可走）。
    fn name(&self) -> &'static str;

    /// 本服务支持的方法名。面板据此判断可用性；注册表据此拒绝未声明的方法
    /// （声明表是模块自己给的，没声明就是"没有这个方法"，不该送进去让模块
    /// 回一个笼统的错误）。
    fn methods(&self) -> &'static [&'static str];

    /// 执行一次调用。
    ///
    /// `method` 必在 [`Self::methods`] 里（注册表已过滤）；`payload` 形状由
    /// 服务自己约定。返回 `Ok(Value)` 即回执的 `data`，`Err(String)` 是给
    /// 人看的原因（进回执的 `error`）。
    async fn invoke(
        &self,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
}

/// 注册表给的**登记面**：模块只能登记，不能遍历别的服务。
pub trait PanelServiceRegistrar: Send + Sync {
    /// 登记一个服务。
    ///
    /// **同名覆盖**：模块重启后会把上一轮的服务重新登记一次，这必须是覆盖而
    /// 不是冲突失败（`Ctx::provide` 对同名是冲突，所以注册表内部自带一层表）。
    /// 返回的 [`Disposer`] 只撤销**本次**这一代——旧句柄晚到的析构不会把新
    /// 登记的顶掉。
    ///
    /// 空服务名、空方法名、重复方法名都返回 `Err`：这些是模块写错了，越早
    /// 报越好（面板侧的表现是"服务存在但方法 404"，排查要翻三个 crate）。
    fn register(&self, service: Arc<dyn PanelService>) -> Result<Disposer, String>;

    /// 撤销某个服务名（模块卸载 / 被禁用时由 disposer 调用）。
    ///
    /// 不存在的服务名是空操作——卸载路径可以无脑调用。
    fn unregister(&self, name: &str);
}

/// 命令泵用的**分发面**：按服务名 + 方法名执行一次调用。
#[async_trait]
pub trait PanelServiceInvoker: Send + Sync {
    /// 分发一次调用。
    ///
    /// 三层拒绝都回**可读原因**（面板侧用户看到的可能就是这句话）：
    /// 服务名没登记 → 带可用清单；方法没声明 → 带该方法清单；服务自己失败
    /// → 原样透传。
    async fn invoke(
        &self,
        service: &str,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
}

/// 面板服务的注册表键（提供方 = Core 组合根，消费方 = 任意模块）。
///
/// 与 trait 同住定义层：两侧都引用这一个常量，谁都不写第二个字面量——键名
/// 漂移的表现是"模块登记了、面板问不到"，只有一份才防得住。
pub static PANEL_SERVICE_REGISTRAR: ServiceKey<Arc<dyn PanelServiceRegistrar>> =
    ServiceKey::new("echo-agent.host.panel_service_registrar");

/// 分发面的键（提供方 = Core 组合根，消费方 = 命令泵）。
///
/// 同一张表 provide 两次（两个 `Arc<dyn Trait>`）——注册面与分发面是两个
/// 不同的能力，谁拿到哪一个是显式的。
pub static PANEL_SERVICE_INVOKER: ServiceKey<Arc<dyn PanelServiceInvoker>> =
    ServiceKey::new("echo-agent.host.panel_service_invoker");

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个最小服务：证明 trait 可以手写实现（不依赖任何模块）。
    struct Echo {
        name: &'static str,
        methods: &'static [&'static str],
    }

    #[async_trait]
    impl PanelService for Echo {
        fn name(&self) -> &'static str {
            self.name
        }
        fn methods(&self) -> &'static [&'static str] {
            self.methods
        }
        async fn invoke(
            &self,
            method: &str,
            payload: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            if !self.methods.contains(&method) {
                return Err(format!("未声明的方法 {method}"));
            }
            Ok(serde_json::json!({ "method": method, "payload": payload }))
        }
    }

    fn echo() -> Arc<dyn PanelService> {
        Arc::new(Echo {
            name: "echo-agent.sessions",
            methods: &["list"],
        })
    }

    #[tokio::test]
    async fn service_is_usable_as_a_trait_object() {
        let service = echo();
        assert_eq!(service.name(), "echo-agent.sessions");
        assert_eq!(service.methods(), &["list"]);
        let value = service
            .invoke("list", serde_json::json!({ "team_id": "alix" }))
            .await
            .unwrap();
        assert_eq!(value["method"], "list");
        assert_eq!(value["payload"]["team_id"], "alix");
    }

    #[tokio::test]
    async fn undeclared_method_is_rejected_by_the_implementation() {
        let error = echo()
            .invoke("nope", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(error.contains("nope"), "{error}");
    }

    /// 键名是跨 crate 的字符串契约：改名等于所有模块与面板同时失联。
    #[test]
    fn keys_are_frozen_configuration_contracts() {
        assert_eq!(
            PANEL_SERVICE_REGISTRAR.name(),
            "echo-agent.host.panel_service_registrar"
        );
        assert_eq!(
            PANEL_SERVICE_INVOKER.name(),
            "echo-agent.host.panel_service_invoker"
        );
    }

    /// 注册面与分发面必须是**两个**键：同一份实现 provide 两次是为了让权限
    /// 显式，键名撞了那个意图就没了。
    #[test]
    fn registrar_and_invoker_keys_differ() {
        assert_ne!(
            PANEL_SERVICE_REGISTRAR.name(),
            PANEL_SERVICE_INVOKER.name()
        );
    }
}
