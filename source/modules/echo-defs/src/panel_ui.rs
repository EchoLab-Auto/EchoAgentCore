//! 面板 UI 贡献接缝：模块把自己的**前端 bundle** 登记给 Core，面板运行期加载。
//!
//! ## 它替掉的是什么
//!
//! 2026-11 之前，模块的 Panel UI 是**构建期**注入的：`PANEL_UI_EXTENSIONS`
//! 环境变量列出模块源码目录，vite 把它们编进 Panel 产物。三个后果都是结构性的：
//!
//! 1. **改一行 UI 要重建整个 Panel**（还要重启服务才能生效）；
//! 2. **忘了配环境变量就静默变纯壳**——模块在 Core 里挂着、界面上什么入口都
//!    没有，构建与启动都不报错（2026-11 实测踩到）；
//! 3. Panel 必须能读到模块源码目录（同一台机器、同一个 checkout），否则装不了。
//!
//! 本接缝把方向反过来：模块产出**标准 ES bundle**（自己 `npm run build`），
//! `mount` 时登记一个 URL；面板启动时拉清单，用 `import()` 加载。
//!
//! ## 归属跟着挂载走
//!
//! 与 [`crate::skill::SkillRegistrar`] / [`crate::panel_service::PanelServiceRegistrar`]
//! 同一套纪律：模块 `mount` 时登记，卸载时 `Disposer` 撤销。模块被禁用 →
//! 面板再也拿不到它的 UI 贡献 → 入口自然消失。**不需要**第二份"哪些模块有
//! UI"的清单（那种清单必然漂移）。
//!
//! ## 为什么登记的是 URL 而不是 bundle 内容
//!
//! bundle 由静态服务托管（`<panel static_dir>/panel-extensions/<id>/`），
//! 面板与 Core 本来就有 HTTP 通道。把几 MB 的 JS 塞进 WS 控制帧只会让协议
//! 变成文件传输层，且丢掉浏览器的模块缓存与 source map。

use std::sync::Arc;

use echo_context::ServiceKey;

use crate::disposer::Disposer;

/// 一份面板 UI 贡献。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelUiContribution {
    /// 扩展 id（与 `registerPanelExtension` 里的一致，如 `echo-agent.workspace`）。
    ///
    /// 面板用它做去重与作用域：**同一个 id 只能有一份**（后登记的覆盖先来的，
    /// 与 [`PanelUiRegistrar::register`] 的覆盖语义一致）。
    pub id: String,
    /// 展示名（面板的设置页/诊断页用它列"已加载的 UI 模块"）。
    pub name: String,
    /// bundle 的**站点相对路径**（如 `panel-extensions/echo-agent.workspace/index.js`）。
    ///
    /// 必须是相对路径：绝对 URL 会让一个被投毒的模块描述把面板变成任意脚本
    /// 加载器（同一个理由下 `InvokeModuleOp` 被归为 Frontend-only）。
    pub bundle: String,
    /// 模块构建时用的 SDK 版本（见 [`PANEL_UI_SDK_VERSION`]）。
    ///
    /// 面板加载前逐字比对，不符即拒绝并给出可读原因。**没有"尽量兼容"**：
    /// UI 扩展在运行期加载，API 用错不会在构建期报错，只会在用户浏览器里
    /// 崩——宁可拒绝加载并说清"该模块需重新构建"，也不要半残的界面。
    pub sdk_version: String,
}

/// 当前 SDK 契约版本。
///
/// **改任何一条面板 SDK 的公开面（导出名、组件 props、`PanelExtension` 字段）
/// 都要改这个值。** 它是面板与模块之间唯一的版本握手，漏改的表现是旧模块
/// 在新面板上白屏。
pub const PANEL_UI_SDK_VERSION: &str = "1";

/// 登记面：模块在自己的 `mount` 里交出 UI 贡献。
pub trait PanelUiRegistrar: Send + Sync {
    /// 登记（或覆盖）一份 UI 贡献。
    ///
    /// **同名覆盖**：模块重启后会把上一轮的贡献重新登记一次，那是常态。
    /// 返回的 [`Disposer`] 只撤销**自己那一代**——"模块 A 重启 → 新贡献登记
    /// → 旧 disposer 晚到析构"不会把新的顶掉。
    ///
    /// 校验（都返回可读原因，因为这些错误最终会显示在面板的诊断页里）：
    /// - `id` 非空、不含空白与 `/`；
    /// - `bundle` 非空、**不是**绝对 URL（含 `://`）或协议相对（`//`）、
    ///   不含 `..`；
    /// - `sdk_version` 非空。
    fn register(&self, contribution: PanelUiContribution) -> Result<Disposer, String>;

    /// 撤销某个 id（模块卸载时由 disposer 调用；不存在的 id 是空操作）。
    fn unregister(&self, id: &str);
}

/// UI 贡献注册表的键（提供方 = Core 组合根，消费方 = 任意模块）。
pub static PANEL_UI_REGISTRAR: ServiceKey<Arc<dyn PanelUiRegistrar>> =
    ServiceKey::new("echo-agent.host.panel_ui_registrar");

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> PanelUiContribution {
        PanelUiContribution {
            id: "echo-agent.workspace".into(),
            name: "工作区会话".into(),
            bundle: "panel-extensions/echo-agent.workspace/index.js".into(),
            sdk_version: PANEL_UI_SDK_VERSION.into(),
        }
    }

    #[test]
    fn contribution_is_plain_data() {
        let contribution = valid();
        assert_eq!(contribution.id, "echo-agent.workspace");
        assert_eq!(contribution.bundle, "panel-extensions/echo-agent.workspace/index.js");
    }

    /// SDK 版本是**跨仓契约**：面板与模块两侧都写死它，改名/改版本号等于
    /// 所有已构建的模块 UI 同时失效（这正是我们要的——宁拒绝不半残）。
    #[test]
    fn sdk_version_is_frozen_contract() {
        assert_eq!(PANEL_UI_SDK_VERSION, "1");
    }

    #[test]
    fn registrar_key_is_frozen_contract() {
        assert_eq!(
            PANEL_UI_REGISTRAR.name(),
            "echo-agent.host.panel_ui_registrar"
        );
    }
}
