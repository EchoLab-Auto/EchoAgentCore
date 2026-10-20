//! 宿主能力的服务键：技能表是 **per-persona** 的，只有宿主知道"现在有哪些
//! 人格、各人的表长什么样"。键属于需求方（本插件），宿主按同一批键注册。

use std::sync::Arc;

use echo_context::ServiceKey;

use crate::commands::SkillsCommandHost;

/// 技能命令域的宿主接缝（表 / 写目录 / 重载 / 启停持久化 / 事件出口）。
pub static SKILLS_HOST: ServiceKey<Arc<dyn SkillsCommandHost>> =
    ServiceKey::new("echo-agent.host.skills");
