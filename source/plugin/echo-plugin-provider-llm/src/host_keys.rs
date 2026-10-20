//! 宿主能力的服务键：供应商池的状态迁移（读配置 / 重建 provider / 落盘 /
//! 广播）只有宿主做得了——它持有 `AgentConfig` 与重建链。

use std::sync::Arc;

use echo_context::ServiceKey;

use crate::commands::ProviderCommandHost;

/// API / provider 命令域的宿主接缝。
pub static PROVIDER_HOST: ServiceKey<Arc<dyn ProviderCommandHost>> =
    ServiceKey::new("echo-agent.host.provider");
