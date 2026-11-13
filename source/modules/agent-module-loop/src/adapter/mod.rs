//! 外部模块贡献的领域适配（原 echo-module-adapter，并入统一 Agent 模块）。

pub mod contract;
pub mod instance;
pub mod skills;
pub mod tool;

pub use contract::{
    IncomingMessage, InstanceCapabilities, InstanceContribution, SkillContribution,
    ToolContribution,
};
pub use echo_module_host::api::Contribution;
pub use instance::{
    message_to_incoming, CoreApiService, InstanceStateChange, InstanceUiStateChange,
    RemoteInstance, RemoteInstanceManagement,
};
pub use skills::ModuleHandleSkills;
pub use tool::{ModuleHandleTools, RemoteTool};

use echo_module_host::bootstrap::StartedModules;
use echo_module_host::ModuleHandle;
use std::sync::Arc;

pub fn tools_of(started: &StartedModules) -> Vec<Arc<RemoteTool>> {
    started
        .handles
        .iter()
        .flat_map(|h| h.remote_tools())
        .collect()
}

/// 全部已启动模块贡献的技能，`owner` = 模块 id（技能表按 owner 整体替换）。
///
/// 与 [`tools_of`] 同一形状：模块句柄是唯一真值来源，这里只做收集。
pub fn skills_of(started: &StartedModules) -> Vec<(String, Vec<echo_defs::skill::Skill>)> {
    started
        .handles
        .iter()
        .map(|h| h.contributed_skills())
        .filter(|(_, skills)| !skills.is_empty())
        .collect()
}

pub fn instance_contributions(
    started: &StartedModules,
) -> Vec<(InstanceContribution, ModuleHandle)> {
    started
        .handles
        .iter()
        .flat_map(|h| {
            h.contributions()
                .into_iter()
                .filter_map(|c| contract::instance_from_custom(&c).map(|i| (i, h.clone())))
                .collect::<Vec<_>>()
        })
        .collect()
}
