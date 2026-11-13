//! 宿主能力的服务键：技能表是 **per-persona** 的，只有宿主知道"现在有哪些
//! 人格、各人的表长什么样"。键属于需求方（本模块），宿主按同一批键注册。

use std::sync::Arc;

use echo_context::ServiceKey;

use crate::commands::SkillsCommandHost;
use crate::Skill;

/// 技能命令域的宿主接缝（表 / 写目录 / 重载 / 启停持久化 / 事件出口）。
pub static SKILLS_HOST: ServiceKey<Arc<dyn SkillsCommandHost>> =
    ServiceKey::new("echo-agent.host.skills");

/// Agent 提供的通用提示词存储（模块只写自己的 owner 命名空间）。
pub static PROMPT_STORE: ServiceKey<echo_defs::prompt::SharedPromptStore> =
    ServiceKey::new("echo-agent.host.prompt_store");

pub static PROMPT_REFRESHER: ServiceKey<echo_defs::prompt::SharedPromptRefresher> =
    ServiceKey::new("echo-agent.host.prompt_refresher.skills");

pub static SKILLS_RUNTIME: ServiceKey<Arc<crate::runtime::SkillsRuntime>> =
    ServiceKey::new("echo-agent.skills.runtime");

/// 外部模块贡献的技能从哪来——**进程级**，不是 per-persona。
///
/// ## 为什么需要这层而不是直接从模块句柄读
///
/// 技能模块的 `kind` 是 `skill`，而 `ModuleHost::PERSONA_SCOPED_KINDS`
/// 把它按人格各挂一份：每个 `Agent` 有自己的 `SkillRegistry`。外部模块
/// （qq / subagent / prodoc）是**进程级**的，一份贡献要出现在所有人格的表
/// 里。于是「模块侧读一次、宿主侧分发到 N 份表」——这是本 trait 的形状：
/// 由组合根实现一次（读已启动模块的贡献），挂在根作用域，人格作用域经
/// `Ctx` 父链继承。
///
/// 反转成」技能模块自己去扫 `StartedModules` 不行：模块 crate 不能依赖
/// 组合根，而 `ModuleHost`（本 crate 的宿主）也不认识 core 的模块运行时。
pub trait SkillContributionSource: Send + Sync {
/// 反转成「技能模块自己去扫 `StartedModules`」不行：模块 crate 不能依赖
    ///
    /// 每次调用返回**当前完整集合**（不要复用旧快照）：模块启停、热替换、
    /// 包门控都会改变它，而 `SkillsRuntime::reload` 用「哪些 owner 这次没
    /// 出现」来判断该撤掉谁。
    fn contributed_skills(&self) -> Vec<(String, Vec<Skill>)>;
}

/// 服务键：组合根注册 [`SkillContributionSource`] 实现。
pub static SKILL_CONTRIBUTION_SOURCE: ServiceKey<Arc<dyn SkillContributionSource>> =
    ServiceKey::new("echo-agent.host.skill_contributions");
