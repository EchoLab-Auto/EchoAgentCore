//! Shell 插件命令域的**宿主能力实现**。
//!
//! `echo-plugin-tools` 的 `ShellCommandHandler` 不认识 `Agent`——它只认识
//! [`ShellCommandHost`] 的四个方法。本模块就是那四个方法的宿主实现。
//!
//! ## 为什么只有"执行命令的那个 agent"，没有 `team_id` 查表
//!
//! 与 workspace 插件的差别：workspace 的 store 是**per-persona 数据**，所以
//! 必须按 `team_id` 取自己那份。Shell 会话管理器是**进程级单例**
//! （`ShellHandle` 克隆共享），会话归属只影响 `ShellSessionInfo.team_id` 这个
//! 字段——而 `apply_command` 在路由阶段已经把带 `team_id` 的命令转发到了正确
//! 的 persona（见 `requires_explicit_team`）。所以"执行者 = 归属者"在这条域上
//! 是**协议保证**，不是猜测；多写一层查表只会多一个可能错的地方。
//!
//! ## `progress` 为什么要单独给
//!
//! `echo_shell::ShellEmit` 是 `echo-shell` 的类型（`Fn(ShellEvent)`），不是
//! 协议事件。插件不该为了它去认识 `BackendEvent` 的构造方式，宿主直接把
//! 「shell 事件 → 面板事件」的翻译闭包交出去（`shell_emit_for_self` 同款）。

use std::sync::Arc;

use echo_plugin_tools::shell_commands::ShellCommandHost;
use echo_protocol::BackendEvent;
use echo_shell::{ShellEmit, ShellManager};

use crate::agent::Agent;

/// 宿主适配器：把"插件要的四件事"接到执行该命令的 `Agent` 上。
pub struct AgentShellCommandHost {
    /// 执行命令的 agent（`apply_command` 已按 `team_id` 路由到它）。
    origin: Arc<Agent>,
}

impl AgentShellCommandHost {
    pub fn new(origin: Arc<Agent>) -> Self {
        Self { origin }
    }
}

impl ShellCommandHost for AgentShellCommandHost {
    fn manager(&self) -> Option<Arc<ShellManager>> {
        self.origin.shell().manager()
    }

    fn emit(&self, event: BackendEvent) {
        // `ShellSessionsList` 自带 `team_id`，其余事件由 `Agent::emit` 的
        // team 标注补齐（插件不接触标注逻辑）。
        self.origin.emit(event);
    }

    fn team_id(&self) -> Option<String> {
        self.origin.team_id()
    }

    fn progress(&self) -> ShellEmit {
        crate::shell::shell_emit_for_self(&self.origin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 归属 = 执行者：`team_id()` 直接取 agent 的 id（协议已保证路由正确）。
    #[tokio::test]
    async fn team_id_comes_from_the_executing_agent() {
        let agent = Arc::new(crate::agent::Agent::new(
            Arc::new(crate::agent::tests::MockProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                reply: String::new(),
            }),
            crate::config::AgentConfig::default(),
            crate::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
        ));
        agent.set_team_id(Some("alice".into()));
        let host = AgentShellCommandHost::new(Arc::clone(&agent));
        assert_eq!(host.team_id().as_deref(), Some("alice"));
        // 未接线时管理器为 None（命令会回可读错误，不静默）。
        assert!(host.manager().is_none());
    }
}
