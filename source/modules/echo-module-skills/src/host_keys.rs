//! 宿主能力的服务键：技能表是 **per-persona** 的，只有宿主知道"现在有哪些
//! 人格、各人的表长什么样"。键属于需求方（本模块），宿主按同一批键注册。

use std::sync::Arc;

use echo_context::ServiceKey;

use crate::commands::SkillsCommandHost;

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
