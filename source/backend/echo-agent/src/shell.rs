//! Shell 子系统的宿主侧接线（实现已外迁到 [`echo_shell`]）。
//!
//! - 机制（`ShellManager` / 哨兵分帧 / 进程组清理 / 事件类型）在 `echo-shell`；
//! - **Agent 耦合的两个函数留在这里**：`shell_emit_for_self` 要拿 `Agent` 的
//!   emit 句柄，`shell_event_to_backend` 只是类型转换（也可在插件侧用）——
//!   留在宿主是因为 `Agent` 属宿主，机制 crate 不该反向认识它。
//!
//! 公开路径 `echo_agent::shell::…` 保持不变（re-export），调用点零改动。

pub use echo_shell::{
    shell_emit_for, shell_event_to_backend, ShellEmit, ShellEvent, ShellHandle, ShellManager,
    MAX_SHELL_SESSIONS,
};

use crate::event::BackendEvent;
use echo_shell::ShellEvent as InnerShellEvent;

/// 面向命令处理层：把事件通到指定 Agent 的 emit。
///
/// 走 `Agent::emit_handle`（快照式安全出口，语义与 emit 一致：team 标注 +
/// 事件总线 + 汇聚点）。2026-10 巡检：此前用 unsafe 原始指针给 `&Agent`
/// 延寿，安全依赖「Arc 由组合根持有」的人工约定；emit_handle 是编译器
/// 见证的 `'static` 出口，正好为此类跨 await 边界调用方而设。
pub fn shell_emit_for_self(agent: &crate::agent::Agent) -> ShellEmit {
    let emit = agent.emit_handle();
    shell_emit_for(move |event: InnerShellEvent| {
        emit(shell_event_to_backend(event));
    })
}

// 显式引用一遍事件类型，确保 re-export 的表面对本模块使用者是完整的。
#[allow(dead_code)]
fn _backend_event_is_used(_: &BackendEvent) {}
