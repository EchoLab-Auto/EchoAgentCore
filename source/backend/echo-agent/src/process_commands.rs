//! **进程级命令**：不属于任何人格、也不该经过 `Agent` 的那些命令。
//!
//! ## 为什么这些命令不该进 `Agent`
//!
//! 改造前 `SaveTeam` / `DeleteTeam` / `ToggleTeam` 走 `Agent::apply_command` 的
//! 分支，内部再 `self.require_admin()` 拿回 `AgentManager` 代办。这有两个问题：
//!
//! 1. **能力放错了层**：人格的增删改是**进程级**操作，但它挂在"某个人格"上；
//! 结果是每个人格都得持有一份管理面引用，而它们永远用不到（命令泵只把命令
//! 交给 `core_agent`）。
//! 2. **静默降级**：`ToggleTeam` 原本是 `None => {}`——未绑定时用户点"启用"
//! 什么都不会发生，也没有日志（2026-10-20 事故的同款形状）。
//!
//! 搬出来之后：`Agent` 的字段里不再有 `admin`，`require_admin()` 删除，
//! `set_host()` 只需绑查询面。**命令泵在路由到 agent 之前先问这里**——问不到
//! 就继续走原来的路径，所以这不是"抢命令"，只是把该在进程层的东西挪回来。
//!
//! ## 与插件命令的关系
//!
//! 顺序是：**插件命令**（S1 的 `Contribution::Command`）→ **进程级命令**（本
//! 模块）→ **人格命令**（`Agent::apply_command` 的内置分支）。插件排最前是因为
//! 它可以被禁用（禁用后自然回落到后面的层，这就是我们要的"命令域归插件"）；
//! 进程级命令排第二是因为它们**没有归属人格**，落到 agent 上只会多绕一圈。

use std::sync::Arc;

use echo_protocol::BackendCommand;

use crate::agent::registry::{AgentRegistry, TeamAdmin};
use crate::agent::Agent;
use crate::event::BackendEvent;

/// 进程级命令的执行环境。
///
/// 三者都是组合根在装配期就能拿到的：`admin` / `registry` 是同一个
/// `AgentManager` 的两个面向，`emitter` 是那个"没有人格身份"的管理面 agent
/// （命令泵本来就把它当默认接收者）。
pub struct ProcessCommandHost {
    /// 团队管理面（增删改人格）。
    pub admin: Arc<dyn TeamAdmin>,
    /// 兄弟查询面（保存后要找回目标 agent 应用新能力）。
    pub registry: Arc<dyn AgentRegistry>,
    /// 事件出口：没有人格归属的 `TeamsList` / `Error` 从这里发。
    pub emitter: Arc<Agent>,
}

impl ProcessCommandHost {
    /// 发一条事件（走管理面 agent 的出口，与改造前一致）。
    fn emit(&self, event: BackendEvent) {
        self.emitter.emit(event);
    }

    fn error(&self, message: impl Into<String>) {
        self.emit(BackendEvent::Error {
            session_id: None,
            message: message.into(),
        });
    }
}

/// 尝试按**进程级**语义处理一条命令。
///
/// 返回 `true` = 已处理（调用方不要再路由给任何 agent）；`false` = 不归这里，
/// 继续走原路径。**不 panic、不吞错**：处理失败一律回可见的 `Error` 事件。
pub async fn try_handle(cmd: &BackendCommand, host: &ProcessCommandHost) -> bool {
    match cmd {
        BackendCommand::SaveTeam { .. } => {
            save_team(cmd, host).await;
            true
        }
        BackendCommand::DeleteTeam { id } => {
            delete_team(host, id).await;
            true
        }
        BackendCommand::ToggleTeam { id, enabled } => {
            toggle_team(host, id, *enabled).await;
            true
        }
        // API 改名：改的是**全局供应商池**（`[agent].api_profiles`），不是某个
        // 人格的私有状态；它还要把 `[agent.teams.*].api_profile` 的引用一起改。
        // 所以它同样是进程级的——但具体逻辑留在 `Agent`（它要动 ModelService
        // 与配置持久化），这里只负责把管理面递进去。
        BackendCommand::RenameApi { from, to } => {
            host.emitter.rename_api(from, to, Some(&host.admin)).await;
            true
        }
        _ => false,
    }
}

/// 保存（新建或覆盖）一个人格，并对运行中的它立即应用新能力。
async fn save_team(cmd: &BackendCommand, host: &ProcessCommandHost) {
    let BackendCommand::SaveTeam {
        id,
        name,
        description,
        system_prompt,
        enabled,
        system_skills,
        disabled_tools,
        disabled_skills,
        enabled_plugins,
        enabled_tools,
        enabled_skills,
        memory_limit_tokens,
        context_window_tokens,
        api_profile,
    } = cmd
    else {
        return;
    };
    let profile = crate::config::AgentProfile {
        system_skills: system_skills.clone(),
        name: name.clone(),
        description: description.clone(),
        system_prompt: system_prompt.clone(),
        enabled: *enabled,
        disabled_tools: disabled_tools.clone(),
        disabled_skills: disabled_skills.clone(),
        enabled_plugins: enabled_plugins.clone(),
        enabled_tools: enabled_tools.clone(),
        enabled_skills: enabled_skills.clone(),
        memory_limit_tokens: *memory_limit_tokens,
        context_window_tokens: *context_window_tokens,
        api_profile: api_profile.clone(),
    };
    if let Err(e) = Arc::clone(&host.admin).save_profile(id, profile.clone(), *enabled) {
        host.error(format!("save agent failed: {e}"));
        return;
    }
    // 勾选变化立即生效：对运行中的目标 agent 重新发现技能文件（覆盖用户手动
    // 新增/修改的 SKILL.md），再应用新的启用/禁用白名单，最后推送技能列表。
    if let Some(target) = host.registry.resolve(id) {
        let dir = target.model.config.read().await.skills_dir.clone();
        if let Err(e) = target.reload_skills(&dir).await {
            tracing::warn!(error = %e, agent = %id, "skill reload after team save failed");
        }
        target.apply_capabilities(&profile).await;
        target.emit_skills_list().await;
        // Persona 级 API 引用变更：立即重建该 persona 的 provider（以管理面
        // agent 的全局配置为池基准）。
        target.set_persona_api(profile.api_profile.clone()).await;
        let global_cfg = host.emitter.model.config.read().await.clone();
        target.apply_persona_api(&global_cfg).await;
        // token 预算热更（2026-10 审计 🔴 修复）：此前只在构造 TrunkStore 时
        // 取一次，保存后运行中 agent 仍用旧预算直到重启。与工厂（main.rs 装配）
        // 同口径：per-persona 覆盖全局 cfg 后再算有效预算（None = 继承）。
        let mut budget_cfg = global_cfg;
        if profile.memory_limit_tokens.is_some() {
            budget_cfg.memory_limit_tokens = profile.memory_limit_tokens;
        }
        if profile.context_window_tokens.is_some() {
            budget_cfg.context_window_tokens = profile.context_window_tokens;
        }
        target
            .trunk
            .set_memory_limit_tokens(budget_cfg.effective_memory_limit_tokens());
        host.emitter.emit_teams_list().await;
    }
    host.error(format!("agent {id} saved"));
}

/// 删除一个人格。
async fn delete_team(host: &ProcessCommandHost, id: &str) {
    match host.admin.delete_profile(id) {
        Ok(()) => {
            host.emitter.emit_teams_list().await;
            host.error(format!("agent {id} deleted"));
        }
        Err(e) => host.error(format!("delete agent failed: {e}")),
    }
}

/// 启用 / 禁用一个已存在的人格。
async fn toggle_team(host: &ProcessCommandHost, id: &str, enabled: bool) {
    let Some(factory) = host.admin.factory_ref() else {
        tracing::warn!(
            team = %id,
            "agent factory not injected; cannot rebuild persona"
        );
        host.error("agent factory not injected; cannot rebuild persona");
        return;
    };
    // `set_enabled` 的接收者是 `Arc<Self>`（它要把 manager 交给重建出来的
    // agent 做绑定），所以 clone 一份。
    match Arc::clone(&host.admin).set_enabled(id, enabled, factory) {
        Ok(()) => {
            // 重新启用的人格：补应用启动期门控（名单 + 全局禁用），否则新实例
            // 以"全开"状态运行到下次保存/重启。
            if enabled {
                if let Some(running) = host.admin.get_running(id) {
                    running.agent.apply_capabilities(&running.profile).await;
                }
            }
            host.emitter.emit_teams_list().await;
            host.error(format!(
                "agent {id} {}",
                if enabled { "enabled" } else { "disabled" }
            ));
        }
        Err(e) => host.error(format!("toggle agent failed: {e}")),
    }
}
