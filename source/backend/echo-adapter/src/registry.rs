//! Adapter registry — manages the lifecycle of multiple adapters.

use std::collections::HashMap;
use std::sync::Arc;

use crate::traits::{Adapter, AdapterError, AdapterInfo};

/// Manages the lifecycle of multiple adapters.
/// Adapter registry.
///
/// 内部 `RwLock<HashMap>`（内部可变性）——`Arc<AdapterRegistry>` 共享下
/// 注册 / 查询 / 删除安全进行（2026-10 适配器插件化：运行期远程适配器
/// 注册需要 `Arc` 下可写）。
pub struct AdapterRegistry {
    adapters: std::sync::RwLock<HashMap<String, Arc<dyn Adapter>>>,
}

impl std::fmt::Debug for AdapterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdapterRegistry")
            .field("names", &self.names())
            .finish()
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self {
            adapters: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Register an adapter. Replaces any existing adapter with the same name.
    ///
    /// 内部可变性（`RwLock`）：`Arc<AdapterRegistry>` 共享下经 `register` 追加
    /// （2026-10 适配器插件化：运行期远程适配器注册需要 `Arc` 下可写）。
    pub fn register(&self, adapter: Arc<dyn Adapter>) {
        self.adapters
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(adapter.name().to_string(), adapter);
    }

    /// Get an adapter by name.
    ///
    /// 按值克隆返回（`RwLock` 读守卫不能跨边界转 `&Arc`；`Arc` 克隆开销可忽略）。
    pub fn get(&self, name: &str) -> Option<Arc<dyn Adapter>> {
        self.adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// List all registered adapters.
    pub fn list_info(&self) -> Vec<AdapterInfo> {
        self.adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|a| a.status_info())
            .collect()
    }

    /// List adapters that are configured (enabled in config).
    pub fn configured(&self) -> Vec<Arc<dyn Adapter>> {
        self.adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|a| a.is_configured())
            .cloned()
            .collect()
    }

    /// Start all configured adapters.
    pub async fn start_all(&self) -> Vec<(String, Result<(), AdapterError>)> {
        let mut results = Vec::new();
        for a in self.configured() {
            let name = a.name().to_string();
            let result = a.start().await;
            results.push((name, result));
        }
        results
    }

    /// Stop all running adapters.
    pub async fn stop_all(&self) {
        let adapters: Vec<_> = self
            .adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for a in adapters {
            let _ = a.stop().await;
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{AdapterConnectionState, AdapterInfo};
    use crate::types::{MessageTarget, SendResult};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct MockAdapter {
        name: String,
        configured: bool,
        started: AtomicBool,
    }

    impl MockAdapter {
        fn new(name: &str, configured: bool) -> Self {
            Self {
                name: name.into(),
                configured,
                started: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl Adapter for MockAdapter {
        fn name(&self) -> &str {
            &self.name
        }
        fn display_name(&self) -> &str {
            &self.name
        }
        fn platform(&self) -> &str {
            "mock"
        }
        fn is_configured(&self) -> bool {
            self.configured
        }
        fn status_info(&self) -> AdapterInfo {
            AdapterInfo {
                name: self.name.clone(),
                display_name: self.name.clone(),
                status: if self.started.load(Ordering::SeqCst) {
                    AdapterConnectionState::Connected
                } else {
                    AdapterConnectionState::Stopped
                },
                self_id: None,
                started_at: None,
                platform: "mock".into(),
                configured: self.configured,
            }
        }
        async fn start(&self) -> Result<(), AdapterError> {
            self.started.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn stop(&self) -> Result<(), AdapterError> {
            self.started.store(false, Ordering::SeqCst);
            Ok(())
        }
        async fn send_message(
            &self,
            _target: &MessageTarget,
            _content: &str,
        ) -> Result<SendResult, AdapterError> {
            Ok(SendResult {
                message_id: Some("1".into()),
                success: true,
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn registry_register_and_get() {
        let reg = AdapterRegistry::new();
        reg.register(Arc::new(MockAdapter::new("test", true)));
        assert!(reg.get("test").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[tokio::test]
    async fn registry_start_configures_only() {
        let reg = AdapterRegistry::new();
        reg.register(Arc::new(MockAdapter::new("enabled", true)));
        reg.register(Arc::new(MockAdapter::new("disabled", false)));

        let results = reg.start_all().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "enabled");
        assert!(results[0].1.is_ok());

        let configured = reg.configured();
        assert_eq!(configured.len(), 1);
    }

    /// 能力扩展默认：未实现 `AdapterManagement` 的适配器返回 `None`
    ///（非适配器上不再存在"调用成功但什么也没发生"的假接口）。
    #[test]
    fn adapter_without_management_returns_none() {
        let mock = MockAdapter {
            name: "tg".into(),
            configured: true,
            started: std::sync::atomic::AtomicBool::new(false),
        };
        assert!(
            mock.management().is_none(),
            "未实现 AdapterManagement 的适配器必须返回 None（显式不支持）"
        );
    }

    #[tokio::test]
    async fn registry_list_info() {
        let reg = AdapterRegistry::new();
        reg.register(Arc::new(MockAdapter::new("a", true)));
        reg.register(Arc::new(MockAdapter::new("b", false)));

        let info = reg.list_info();
        assert_eq!(info.len(), 2);
        assert!(info.iter().any(|i| i.name == "a" && i.configured));
        assert!(info.iter().any(|i| i.name == "b" && !i.configured));
    }
}
