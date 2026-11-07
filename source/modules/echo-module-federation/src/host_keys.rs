//! 联邦服务的服务键：只需要出口集合；事件由服务自己的协议定义。

use crate::ports::FederationPorts;
use echo_context::ServiceKey;
use std::sync::Arc;

pub static PORTS: ServiceKey<Arc<FederationPorts>> = ServiceKey::new("echo-agent.host.federation");

/// 进程级事件出口（组合根翻译 FederationEvent → BackendEvent 后广播）。
pub static EVENTS: ServiceKey<Arc<dyn Fn(crate::service::FederationEvent) + Send + Sync>> =
    ServiceKey::new("echo-agent.host.federation.events");
