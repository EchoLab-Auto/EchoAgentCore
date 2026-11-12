//! # agent-module-transcript — 显示层模块（解耦计划 S8）
//!
//! 面板的**显示投影与命令域**：实时时间线投影（`timeline`）+ 会话视图命令
//! （`session_view`：5 条 `Request*`）。
//!
//! ## 为什么它从 management 里拆出来
//!
//! `agent-module-management` 的职责是**连接面**（WS 中继、鉴权、端口合并、
//! 心跳）。而本 crate 关心的是**"面板看到什么"**——哪些事件进时间线、字段
//! 怎么映射、工具完成态怎么原地合并、推理发往前端怎么瘦身、旧会话历史怎么
//! 回填。两类的变更轴不同：
//!
//! | 变什么 | 改哪个 crate |
//! |---|---|
//! | WS 协议升级 / token 轮换 / 端口冲突 | `agent-module-management` |
//! | 思考条目顺序 / 工具占位名 / 推理瘦身阈值 | **本 crate** |
//!
//! 合在一起意味着两类改动共用一次编译与一次发布；更重要的是，management 的
//! "禁用 = 关闭 Panel 通道"语义会把显示层一起关掉——而显示层的数据源
//! （`TrunkStore`）与面板通道并无必然耦合。
//!
//! ## 宿主接缝只有一件事：数据面
//!
//! - [`timeline::TimelineHost`]：往 `TrunkStore` 写窗口（`push` / `next_seq` /
//!   `complete_running_tool`）
//! - [`session_view::SessionViewHost`]：读快照与派生历史
//!
//! 刻意不暴露 `TrunkStore` 引用：那会让模块认识宿主的锁类型与窗口实现
//! （"seam 变影子"的经典形状）。每个方法都是一次**语义快照**。
//!
//! ## 依赖纪律
//!
//! 只依赖词汇层（`echo-defs` / `agent-module-management` / `echo-session`）与机制层
//! （`echo-context` / `agent-module-federation`），**不依赖任何其它模块**。

use std::sync::Arc;

use echo_context::ServiceKey;
use echo_module::{BuiltinModule, Inject, Module, ModuleManifest, ServiceSlot};

pub mod session_view;
pub mod timeline;

/// 模块 id。
pub const MODULE_ID: &str = "echo-agent.transcript";

/// 会话视图宿主接缝的服务键（与 management 期保持一致：`echo-agent.host.session_view`）。
pub const SESSION_VIEW_HOST_SERVICE: &str = "echo-agent.host.session_view";

/// 会话视图宿主接缝键（`mount` 时绑定，`handle` 时取用）。
pub static SESSION_VIEW_HOST_KEY: ServiceKey<Arc<dyn session_view::SessionViewHost>> =
    ServiceKey::new(SESSION_VIEW_HOST_SERVICE);

/// 构造本模块（catalog 用）。
pub fn module(version: &str) -> Arc<dyn Module> {
    let manifest = ModuleManifest::builtin(
        MODULE_ID,
        "显示层",
        version,
        "interaction",
        "transcript",
        "显示投影：时间线（实时）+ 会话视图命令域（快照 / 上下文 / 翻页 / 历史）",
    );
    let view = ServiceSlot::<Arc<dyn session_view::SessionViewHost>>::new();
    let (bind_view, serve_view) = (view.clone(), view.clone());
    Arc::new(
        BuiltinModule::new(manifest, move |mctx| {
            let keep = bind_view.bind(mctx, &SESSION_VIEW_HOST_KEY)?;
            Ok(vec![keep])
        })
        .with_inject(Inject::new([SESSION_VIEW_HOST_KEY.name().to_string()]))
        .with_commands(session_view::COMMANDS)
        .with_command_handler_async(move |name, payload| {
            let serve_view = serve_view.clone();
            Box::pin(async move {
                let Some(host) = serve_view.get() else {
                    return Err(format!(
                        "命令 {name} 到 {MODULE_ID} 时模块尚未挂载（会话视图接缝未绑定）"
                    ));
                };
                session_view::SessionViewHandler::new(host)
                    .handle(&name, payload)
                    .await
                    .unwrap_or_else(|| Err(format!("命令 {name} 不属于显示层命令域")))
            })
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_id_is_frozen_configuration_contract() {
        assert_eq!(MODULE_ID, "echo-agent.transcript");
    }

    #[test]
    fn manifest_declares_interaction_kind() {
        let module = module("9.9.9");
        let manifest = module.manifest();
        assert_eq!(manifest.kind, "interaction");
        assert_eq!(manifest.id, MODULE_ID);
        assert!(manifest.builtin);
    }

    /// 命令域必须与此前 management 完全一致（5 条），不能丢。
    #[test]
    fn declares_the_session_view_commands() {
        // 5 = 五个会话视图命令。压缩前归档（2026-11）不单独占命令：它挂在
        // `RequestSessionHistory` 的 `expand_depth` 上——归档要与实时历史
        // 拼成**同一条消息流**，分成两条命令就得让面板自己拼边界（见
        // `store::history::display_history_extended`）。
        assert_eq!(session_view::COMMANDS.len(), 5);
        for name in [
            "RequestState",
            "RequestContext",
            "RequestTrunkTimeline",
            "RequestTimelineEarlier",
            "RequestSessionHistory",
        ] {
            assert!(session_view::COMMANDS.contains(&name), "缺 {name}");
        }
    }
}
