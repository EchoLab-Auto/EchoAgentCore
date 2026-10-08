//! 宿主服务注册表：插件经 `HostCall` 回呼宿主内注册的服务（P2）。
//!
//! 宿主侧实现 [`HostService`] 并以名字注册进 [`HostServiceRegistry`]；插件发送
//! `PluginToHost::HostCall { service, method, payload }` 后，supervisor 在
//! 后台任务中查表调用，并把结果（或错误）经 `HostToPlugin::HostCallResult`
//! 回发。调用是同步的（`&self`）——实现方如需阻塞 IO 请自行 spawn / 用异步
//! 接口封装，避免长时间占用调用任务（supervisor 侧不阻塞消息循环）。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde_json::Value;

/// 一个可被插件回呼的宿主服务。
pub trait HostService: Send + Sync {
    /// 方法调用：Ok 返回 JSON 结果；Err = (机器可读错误码, 人类可读消息)。
    ///
    /// 服务内的未知方法应返回 `("method_not_found", …)`（注册表不代查方法名）。
    fn call(&self, method: &str, payload: Value) -> Result<Value, (String, String)>;
}

/// 宿主服务注册表（按名字登记；`Arc` 共享，可 Clone 语义）。
#[derive(Default)]
pub struct HostServiceRegistry {
    services: RwLock<HashMap<String, Arc<dyn HostService>>>,
}

impl HostServiceRegistry {
    /// 空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册（或覆盖）一个服务：同名覆盖时 warn 并替换旧实现。
    pub fn register(&self, name: &str, service: Arc<dyn HostService>) {
        let replaced = self
            .services
            .write()
            .expect("host service registry lock poisoned")
            .insert(name.to_string(), service)
            .is_some();
        if replaced {
            tracing::warn!(
                service = name,
                "host service re-registered; previous implementation replaced"
            );
        } else {
            tracing::debug!(service = name, "host service registered");
        }
    }

    /// 调用一个服务方法。
    ///
    /// 未知服务 → `Err(("service_not_found", …))`；服务内未知方法由各实现
    /// 自行返回 `method_not_found`。
    pub fn call(
        &self,
        service: &str,
        method: &str,
        payload: Value,
    ) -> Result<Value, (String, String)> {
        let registered = self
            .services
            .read()
            .expect("host service registry lock poisoned")
            .get(service)
            .cloned();
        match registered {
            Some(service) => service.call(method, payload),
            None => Err((
                "service_not_found".to_string(),
                format!("unknown host service: {service}"),
            )),
        }
    }
}
