//! 宿主能力的服务键：本域只需要一个事件出口（updater 单元名与状态文件布局
//! 都归插件，宿主不必提供任何东西）。

use std::sync::Arc;

use echo_context::ServiceKey;
use echo_protocol::BackendEvent;

/// 进程级事件出口。
pub static EVENTS: ServiceKey<Arc<dyn Fn(BackendEvent) + Send + Sync>> =
    ServiceKey::new("echo-agent.host.selfupdate.events");
