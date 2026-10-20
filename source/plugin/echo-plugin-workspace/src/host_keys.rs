//! 宿主能力的服务键：工作区 store 是 **per-persona** 的（按 `team_id` 解析），
//! 浏览根要跨全部人格，跨机查询要联邦出口——三件事都只有宿主做得到。

use std::sync::Arc;

use echo_context::ServiceKey;

use crate::commands::WorkspaceHost;

/// 工作区命令域 + 提示词块的宿主接缝。
pub static WORKSPACE_HOST: ServiceKey<Arc<dyn WorkspaceHost>> =
    ServiceKey::new("echo-agent.host.workspace");
