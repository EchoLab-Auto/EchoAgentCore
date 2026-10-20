//! 宿主能力的服务键：联邦出口集合与事件出口都是**装配期构造**的进程级对象
//! （`FederationPorts` 里的每个出口都绑在具体句柄上），只有组合根能造。

use std::sync::Arc;

use echo_context::ServiceKey;
use echo_protocol::BackendEvent;

use crate::ports::FederationPorts;

/// 出口集合（远程调用 / 查询 / 委派 / 摘要投递 / 命���处理）。
pub static PORTS: ServiceKey<Arc<FederationPorts>> = ServiceKey::new("echo-agent.host.federation");

/// 进程级事件出口（联邦命令的回执走它）。
pub static EVENTS: ServiceKey<Arc<dyn Fn(BackendEvent) + Send + Sync>> =
    ServiceKey::new("echo-agent.host.federation.events");
