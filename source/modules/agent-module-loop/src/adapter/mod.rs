//! 外部模块贡献的领域适配（原 echo-module-adapter，并入统一 Agent 模块）。

pub mod contract;
pub mod instance;
pub mod tool;

pub use contract::{IncomingMessage, InstanceCapabilities, InstanceContribution, ToolContribution};
pub use echo_module_host::api::Contribution;
pub use instance::{
    message_to_incoming, CoreApiService, InstanceStateChange, InstanceUiStateChange,
    RemoteInstance, RemoteInstanceManagement,
};
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
