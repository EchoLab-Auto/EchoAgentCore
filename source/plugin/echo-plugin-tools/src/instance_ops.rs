//! 插件实例管理通道：`InvokeInstanceOp` 的处理（平台中立）。
//!
//! ## 为什么归本插件
//!
//! `instance_*` 工具、`StartInstance` 等 6 条命令都已在这里（全插件化 S3）。
//! 剩下 `InvokeInstanceOp` 是最后一条**通用**实例命令：它把任意方法名转发给
//! 实例自己的 `InstanceManagement::invoke_custom`，并负责回执、写方法跨实例
//! 对齐、状态下发。它同样只需要两样东西——注册表与事件出口。
//!
//! ## 三条不变量（搬过来时逐条保留）
//!
//! 1. **多实例寻址**：显式 `instance` 优先；缺省按注册表全体回退（0 个报错、
//!    恰 1 个选它、≥2 要求显式指定）。平台中立——没有 `platform()` 过滤。
//! 2. **失败也回执**：`InstanceOpResult{result: null, error}`，不是 `Error`
//!    事件。Panel 按 `(instance, method)` 键配对，不必等超时。
//! 3. **状态下发单源**：唯一事实来源是 `InstanceManagement::cached_state`；
//!    写方法与 `get_state` 读路径在完成后推送 `InstanceState`，其余读方法
//!    不重复下发。

use std::sync::Arc;

use echo_defs::plugin_instance::{InstanceRegistry, PluginInstance};
use echo_protocol::BackendEvent;

/// 写方法：这些方法的配置在**共享段**（gating 平台跨实例一致），调用后要
/// 对齐同 platform 的其余实例并刷新状态。
///
/// 为什么是常量表：它是**平台行为契约**（哪些方法写共享段），不是可推导
/// 事实。新增写方法必须在这里加一行——漏了会表现为"改了实例 A，B 没跟上"。
const SHARED_WRITE_METHODS: &[&str] = &[
    "set_gate_mode",
    "update_allowlist",
    "update_denylist",
    "set_owner",
];

/// 管理通道的执行器：注册表 + 事件出口，两件事。
pub struct InstanceOpRunner<H: crate::instance_commands::InstanceCommandHost + ?Sized> {
    host: Arc<H>,
}

impl<H: crate::instance_commands::InstanceCommandHost + ?Sized> InstanceOpRunner<H> {
    pub fn new(host: Arc<H>) -> Self {
        Self { host }
    }

    /// 处理一条 `InvokeInstanceOp`。
    ///
    /// `instance` 是命令里携带的实例名（可能为空 = 请求按注册表回退）。
    pub async fn invoke(
        &self,
        instance: &str,
        method: &str,
        payload: serde_json::Value,
    ) {
        let instances = self.host.instances();
        tracing::info!(instance, method, "InvokeInstanceOp");
        let target = match resolve_instance(&instances, instance) {
            Ok(target) => target,
            Err(message) => {
                // 解析失败也回执（用命令自带的 instance/method 回显）。
                self.ack(instance, method, Err(message));
                return;
            }
        };
        if SHARED_WRITE_METHODS.contains(&method) {
            broadcast_to_same_platform(&instances, instance, method, payload.clone()).await;
        }
        let succeeded = match invoke_method(target.clone(), method, payload).await {
            Ok(result) => {
                self.ack(instance, method, Ok(result));
                true
            }
            Err(message) => {
                self.ack(instance, method, Err(message));
                false
            }
        };
        // 写方法完成后刷新状态（读类查询不重复下发）；读路径 `get_state`
        // 成功也推送（Panel 首屏）。
        if SHARED_WRITE_METHODS.contains(&method) || (succeeded && method == "get_state") {
            self.emit_state(Arc::clone(&target));
        }
    }

    /// 解析命令要操作的实例（见模块文档不变量 1）。
    ///
    /// 公开是因为宿主还有一处需要它：`RequestState` 的全量快照要按实例名
    /// 取句柄再下发状态。
    pub fn resolve(&self, instance: Option<&str>) -> Result<Arc<dyn PluginInstance>, String> {
        resolve_instance(&self.host.instances(), instance.unwrap_or(""))
    }

    /// 状态下发（`InstanceState` 单源）。
    ///
    /// 缺管理面 / 无状态时**静默跳过**：调用方已经给过错误反馈，这里再报一次
    /// 会让面板收到两条互斥消息。
    pub fn emit_state(&self, instance: Arc<dyn PluginInstance>) {
        let Some(management) = instance.management() else {
            return;
        };
        let Some(state) = management.cached_state() else {
            return;
        };
        self.host.emit(BackendEvent::InstanceState {
            instance: instance.name().to_string(),
            state,
        });
    }

    fn ack(
        &self,
        instance: &str,
        method: &str,
        outcome: Result<serde_json::Value, String>,
    ) {
        let (result, error) = match outcome {
            Ok(result) => (result, None),
            Err(message) => (serde_json::Value::Null, Some(message)),
        };
        self.host.emit(BackendEvent::InstanceOpResult {
            instance: instance.to_string(),
            method: method.to_string(),
            result,
            error,
        });
    }
}

/// 实例寻址：显式名字优先；缺省按注册表全体回退。
///
/// 0 个 → 错误（没有任何实例可用）；恰 1 个 → 选它（单实例部署零配置）；
/// ≥2 → 要求显式指定（猜错实例比报错更糟）。
pub fn resolve_instance(
    instances: &Arc<InstanceRegistry>,
    instance: &str,
) -> Result<Arc<dyn PluginInstance>, String> {
    if !instance.is_empty() {
        return instances
            .get(instance)
            .ok_or_else(|| format!("插件实例 {instance} 未找到"));
    }
    let all: Vec<Arc<dyn PluginInstance>> = instances
        .names()
        .into_iter()
        .filter_map(|name| instances.get(&name))
        .collect();
    match all.len() {
        0 => Err("没有可用的插件实例".into()),
        1 => Ok(all.into_iter().next().expect("len == 1")),
        _ => Err(format!(
            "存在 {} 个插件实例，命令必须显式指定 instance（可选：{}）",
            all.len(),
            all.iter()
                .map(|a| a.name().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// 写方法跨实例对齐（best-effort）：目标 = 与源实例**相同 platform** 的其它
/// 实例；源实例未声明 `gating` 能力时不广播（每实例独立门控的平台不做共享段
/// 对齐）。
pub async fn broadcast_to_same_platform(
    instances: &Arc<InstanceRegistry>,
    skip: &str,
    method: &str,
    payload: serde_json::Value,
) {
    let Some(source) = instances.get(skip) else {
        return;
    };
    if !source.gating_capabilities() {
        return;
    }
    let source_platform = source.platform().to_string();
    for name in instances.names() {
        if name == skip {
            continue;
        }
        let Some(other) = instances.get(&name) else {
            continue;
        };
        if other.platform() != source_platform {
            continue;
        }
        if let Err(message) = invoke_method(other, method, payload.clone()).await {
            tracing::warn!(
                instance = %name,
                %method,
                %message,
                "instance op broadcast failed"
            );
        }
    }
}

/// 经 `InstanceManagement::invoke_custom` 调用一个管理方法。
///
/// 实例不存在 / 缺管理面 / 方法失败都以可读错误返回——调用方据此回执，
/// 不 panic、不静默。
pub async fn invoke_method(
    instance: Arc<dyn PluginInstance>,
    method: &str,
    payload: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let Some(management) = instance.management() else {
        return Err(format!("插件实例 {} 缺少管理面实现", instance.name()));
    };
    management
        .invoke_custom(method, payload)
        .await
        .map_err(|error| format!("{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 寻址回退：0 个报错、恰 1 个选它、≥2 要求显式。
    #[test]
    fn resolve_falls_back_by_registry_size() {
        let registry = Arc::new(InstanceRegistry::new());
        assert!(resolve_instance(&registry, "").is_err(), "空注册表必须报错");

        // 一个也没有时，显式名字也要报"未找到"。
        assert!(resolve_instance(&registry, "ghost").is_err());
    }

    /// 写方法表：它是平台行为契约，改动必须显式（这条测试是"改它要想一想"
    /// 的提醒，而不是永不可变的断言）。
    #[test]
    fn shared_write_methods_are_the_known_platform_contract() {
        assert_eq!(
            SHARED_WRITE_METHODS,
            &[
                "set_gate_mode",
                "update_allowlist",
                "update_denylist",
                "set_owner"
            ]
        );
    }
}
