//! 面板服务注册表：模块登记能力，面板按**服务名**请求。
//!
//! 契约在 [`echo_defs::panel_service`]（trait + 键），本文件是它在 Core 侧的
//! 唯一实现。
//!
//! ## 为什么注册表由组合根实现
//!
//! 它要活到进程结束、要能被**所有**模块在自己 `mount` 里登记、又要能被命令泵
//! 分发时查到——三件事都要求"比任何模块活得久、且双方都能看见"。组合根是
//! 唯一满足的位置（与 [`crate::host_services`] 同一理由）。
//!
//! 顺序要求：本表必须在**任何模块 mount 之前** provide 进 `Ctx`。晚了的
//! 表现是模块 `mount` 里 `service()` 返回 `None`——按设计那不是致命错误
//! （服务是可选的，嵌入式部署没有宿主），所以这类 bug 会**静默**：
//! 模块照常挂载，只是它的服务永远查不到。因此登记侧一律打 debug 日志，
//! 便于"服务没登记上"与"面板拼错服务名"两种情况分开排查。
//!
//! ## 与 `InvokeModuleOp` 的分工
//!
//! `InvokeModuleOp` 按**模块 id** 寻址（"我要调 `agent-module-session` 的某个
//! 方法"），面板因此必须知道实现者是谁。本表按**服务名**寻址（"谁会列会话"），
//! 提供方可以换模块、可以改模块名，面板不用动。
//!
//! 两者并存不是冗余，判据是**谁该为名字负责**：模块的私有方法归模块
//! （`workspace.deleteConversation` 这种点分小写词汇），面板要读的领域概念
//! 归服务名（`echo-agent.sessions`）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use echo_defs::panel_service::{PanelService, PanelServiceInvoker, PanelServiceRegistrar};
use echo_defs::Disposer;

/// 一个已登记服务的槽位。
struct Entry {
    /// 登记代次：同名覆盖时递增。`Disposer` 只撤销**自己那一代**——否则
    /// "模块 A 重启 → 新服务登记 → 旧 disposer 晚到析构"会把新的顶掉
    /// （`Ctx::register` 用同一个手法处理这件事）。
    generation: u64,
    service: Arc<dyn PanelService>,
}

/// 注册表本体：名字 → 服务。
///
/// **不实现** `PanelServiceRegistrar`：`register` 要返回捕获了 `Arc<Self>` 的
/// `Disposer`，而 `&self` 拿不到 `Arc`。实现类在下面的 [`TableHandle`]。
#[derive(Default)]
pub(crate) struct PanelServiceTable {
    entries: RwLock<HashMap<String, Entry>>,
    next_generation: AtomicU64,
}

impl std::fmt::Debug for PanelServiceTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelServiceTable")
            .field("services", &self.names())
            .finish()
    }
}

impl PanelServiceTable {
    /// 空表。调用方随后用 [`Self::registrar`] / [`Self::invoker`] 造出要
    /// provide 进 `Ctx` 的句柄。
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 造一个可放进 `Ctx` 的登记句柄（与本体共享同一张表）。
    ///
    /// 只暴露 [`PanelServiceRegistrar`]：模块能登记，但不能遍历别人的服务。
    pub(crate) fn registrar(self: &Arc<Self>) -> Arc<dyn PanelServiceRegistrar> {
        Arc::new(TableHandle {
            table: Arc::clone(self),
        })
    }

    /// 造一个可放进 `Ctx` 的分发句柄（命令泵用）。
    pub(crate) fn invoker(self: &Arc<Self>) -> Arc<dyn PanelServiceInvoker> {
        Arc::new(InvokeHandle {
            table: Arc::clone(self),
        })
    }

    /// 当前全部服务名（排序；诊断与错误消息用）。
    pub(crate) fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// 按服务名找一个服务。
    pub(crate) fn get(&self, name: &str) -> Option<Arc<dyn PanelService>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .map(|entry| entry.service.clone())
    }

    /// 只在当前槽位仍是 `generation` 这一代时删除（旧 disposer 不能顶掉新的）。
    fn remove_if_generation(&self, name: &str, generation: u64) {
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = entries.get(name).map(|entry| entry.generation);
        if current != Some(generation) {
            return;
        }
        entries.remove(name);
        drop(entries);
        tracing::debug!(service = name, "面板服务已撤销");
    }

    /// 分发一次调用（**命令泵面向注册表的唯一入口**）。
    ///
    /// 三层拒绝，每层都回可读原因——面板侧用户看到的就是这句话，所以不能
    /// 只回一句 404：
    ///
    /// 1. 服务名没登记 → 回可用服务清单（"你是不是拼错了"）；
    /// 2. 服务在但方法没声明 → 回该服务的方法清单；
    /// 3. 服务自己 `Err` → 原样透传（模块的领域原因，注册表不加工）。
    pub(crate) async fn invoke(
        &self,
        service: &str,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let Some(found) = self.get(service) else {
            let available = self.names();
            return Err(if available.is_empty() {
                format!("未登记任何面板服务（请求 {service}.{method}）")
            } else {
                format!(
                    "面板服务 {service} 不存在（请求 {service}.{method}）；已登记：{}",
                    available.join(", ")
                )
            });
        };
        if !found.methods().contains(&method) {
            return Err(format!(
                "面板服务 {service} 未声明方法 {method}；可用方法：{}",
                found.methods().join(", ")
            ));
        }
        found.invoke(method, payload).await
    }
}

/// 放进 `Ctx` 的登记句柄——**只暴露登记面**，不能遍历别人的服务。
struct TableHandle {
    table: Arc<PanelServiceTable>,
}

impl PanelServiceRegistrar for TableHandle {
    fn register(&self, service: Arc<dyn PanelService>) -> Result<Disposer, String> {
        let name = service.name();
        if name.trim().is_empty() {
            return Err("面板服务名不能为空".to_string());
        }
        if name.len() > 128 {
            return Err(format!("面板服务名过长（{} 字节）：{name}", name.len()));
        }
        let methods = service.methods();
        if methods.is_empty() {
            return Err(format!("面板服务 {name} 没有声明任何方法"));
        }
        for (index, method) in methods.iter().enumerate() {
            if method.trim().is_empty() {
                return Err(format!("面板服务 {name} 声明了空方法名"));
            }
            if methods[..index].contains(method) {
                return Err(format!("面板服务 {name} 重复声明方法 {method}"));
            }
        }
        let generation = self.table.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let previous = self
            .table
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                name.to_string(),
                Entry {
                    generation,
                    service,
                },
            );
        if previous.is_some() {
            // 同名覆盖是**设计**（模块重启后重新登记），但值得记一条：若是
            // 同一部署里两个模块抢同一个名字，这就是唯一线索。
            tracing::debug!(
                service = name,
                methods = %methods.join(","),
                "面板服务被覆盖登记（同名服务的新一代）"
            );
        } else {
            tracing::debug!(
                service = name,
                methods = %methods.join(","),
                "面板服务已登记"
            );
        }
        let table = Arc::clone(&self.table);
        let owned = name.to_string();
        Ok(Disposer::from_fn(move || {
            table.remove_if_generation(&owned, generation);
        }))
    }

    fn unregister(&self, name: &str) {
        self.table
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
    }
}

/// 放进 `Ctx` 的分发句柄——命令泵只拿它，**没有**登记能力。
struct InvokeHandle {
    table: Arc<PanelServiceTable>,
}

#[async_trait::async_trait]
impl PanelServiceInvoker for InvokeHandle {
    async fn invoke(
        &self,
        service: &str,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.table.invoke(service, method, payload).await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct Stub {
        name: &'static str,
        methods: &'static [&'static str],
    }

    #[async_trait]
    impl PanelService for Stub {
        fn name(&self) -> &'static str {
            self.name
        }
        fn methods(&self) -> &'static [&'static str] {
            self.methods
        }
        async fn invoke(
            &self,
            method: &str,
            payload: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            if method == "boom" {
                return Err("模块侧原因".to_string());
            }
            Ok(serde_json::json!({ "method": method, "payload": payload }))
        }
    }

    fn stub(name: &'static str, methods: &'static [&'static str]) -> Arc<dyn PanelService> {
        Arc::new(Stub { name, methods })
    }

    #[tokio::test]
    async fn registered_service_is_dispatchable() {
        let table = PanelServiceTable::new();
        let handle = table.registrar();
        let _keep = handle
            .register(stub("echo-agent.sessions", &["list"]))
            .unwrap();
        assert_eq!(table.names(), vec!["echo-agent.sessions"]);
        let value = table
            .invoke("echo-agent.sessions", "list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(value["method"], "list");
    }

    /// 未登记的服务名：错误里必须带**可用清单**，否则面板侧只能盲猜。
    #[tokio::test]
    async fn unknown_service_lists_what_is_available() {
        let table = PanelServiceTable::new();
        let _keep = table
            .registrar()
            .register(stub("echo-agent.sessions", &["list"]))
            .unwrap();
        let error = table
            .invoke("echo-agent.sesions", "list", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(error.contains("echo-agent.sesions"), "{error}");
        assert!(error.contains("echo-agent.sessions"), "{error}");
    }

    /// 没说的方法：错误里必须带该服务的方法清单。
    #[tokio::test]
    async fn undeclared_method_lists_the_service_methods() {
        let table = PanelServiceTable::new();
        let _keep = table
            .registrar()
            .register(stub("echo-agent.sessions", &["list", "get"]))
            .unwrap();
        let error = table
            .invoke("echo-agent.sessions", "list_all", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(error.contains("list_all"), "{error}");
        assert!(error.contains("list, get"), "{error}");
    }

    /// 服务自己失败：原因原样透传，注册表不包装（否则领域原因会被淹没）。
    #[tokio::test]
    async fn service_failure_is_passed_through() {
        let table = PanelServiceTable::new();
        let _keep = table
            .registrar()
            .register(stub("echo-agent.sessions", &["boom"]))
            .unwrap();
        let error = table
            .invoke("echo-agent.sessions", "boom", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert_eq!(error, "模块侧原因");
    }

    /// 同名覆盖：新一代接管，旧代次的 disposer 晚到析构**不能**删掉新的。
    #[tokio::test]
    async fn stale_disposer_does_not_remove_the_new_generation() {
        let table = PanelServiceTable::new();
        let handle = table.registrar();
        let old = handle
            .register(stub("echo-agent.sessions", &["list"]))
            .unwrap();
        let new = handle
            .register(stub("echo-agent.sessions", &["list", "get"]))
            .unwrap();
        drop(old);
        assert!(table.get("echo-agent.sessions").is_some(), "旧代次析构不应删除新登记");
        assert_eq!(
            table.get("echo-agent.sessions").unwrap().methods(),
            &["list", "get"]
        );
        drop(new);
        assert!(table.get("echo-agent.sessions").is_none());
    }

    /// 模块写错声明：早报，别等到面板拿到 404 才查三个 crate。
    #[test]
    fn malformed_declarations_are_rejected() {
        let table = PanelServiceTable::new();
        let handle = table.registrar();
        assert!(handle.register(stub("", &["list"])).is_err());
        assert!(handle.register(stub("echo-agent.sessions", &[])).is_err());
        assert!(handle
            .register(stub("echo-agent.sessions", &["list", "list"]))
            .is_err());
        assert!(handle.register(stub("echo-agent.sessions", &[""])).is_err());
    }

    /// 撤销是幂等的：卸载路径可以无脑调。
    #[test]
    fn unregister_is_idempotent() {
        let table = PanelServiceTable::new();
        let handle = table.registrar();
        let keep = handle
            .register(stub("echo-agent.sessions", &["list"]))
            .unwrap();
        handle.unregister("echo-agent.sessions");
        handle.unregister("echo-agent.sessions");
        assert!(table.names().is_empty());
        drop(keep);
        assert!(table.names().is_empty());
    }
}
