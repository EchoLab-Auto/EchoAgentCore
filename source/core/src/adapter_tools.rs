//! 适配器工具的**通用桥**：把适配器声明的工具注册给模型，并把调用透传回去。
//!
//! **平台语义不在本仓库**：工具名 / 描述 / 参数 schema 全部来自
//! [`echo_defs::adapter::Adapter::tool_specs`]（远程适配器 = 插件
//! `AdapterContribution.tools` 原样）。本模块只做三件与平台无关的事：
//!
//! 1. **合并**：同一 persona 的多个实例声明同一工具 → 只注册一次；
//! 2. **寻址**：多实例时给 schema 注入可选 `account`，执行时按它选实例
//!    （`account` 是宿主参数，不进入插件 payload）；
//! 3. **过闸**：所有适配器工具都跨平台边界，统一走 [`OutboundGate`]
//!    （敏感文本按策略阻断/替换；文件参数命中敏感目录直接拒发）。
//!
//! 执行 = `invoke_custom(<工具名>, <参数>)`（见 [`invoke_management`]）。
//! 工具必须实现 `echo_agent::Tool`，所以桥住在本 crate（插件刻意不依赖
//! echo-agent）。

use std::sync::Arc;

use async_trait::async_trait;
use echo_adapter::traits::Adapter;
use serde_json::{json, Value};

use crate::security::OutboundPolicy;

/// 适配器出站闸门（2026-10 安全）：适配器是消息出站的直接通道，发送前统一检查。
///
/// - 文本命中脱敏器 → 按 [`OutboundPolicy`] 阻断（默认）或替换后照发；
/// - 文件类参数（`file_path` / `file` / `image`）落在敏感目录（配置目录 /
///   `~/.ssh` 等）→ 一律拒发（防"整文件打包外发"）。
///
/// `pattern` 命中来自脱敏器：注册密钥（精确值）+ 保守模式集。
pub struct OutboundGate {
    /// 脱敏器（进程级共享实例）。
    pub redactor: Arc<dyn echo_defs::sanitize::Redactor>,
    /// 受保护目录（文件外发直接拒发；命中判定含符号链接解析）。
    pub protected_paths: Vec<std::path::PathBuf>,
    /// 文本命中策略。
    pub policy: OutboundPolicy,
}

/// 该 persona 的适配器实例集合（多实例寻址）。
///
/// 适配器句柄为 `Arc<dyn Adapter>`（P1.8 适配器插件化）：独立进程的插件
/// 实例（`RemoteAdapter`）——平台专属动作统一经
/// [`invoke_management`]（管理面 `invoke_custom`）路由。
#[derive(Clone)]
struct InstanceSet {
    adapters: Vec<Arc<dyn Adapter>>,
    /// 缺省实例（恰好一个实例时即它；多实例时 None → 必须给 account）。
    default_index: Option<usize>,
}

impl InstanceSet {
    fn new(adapters: Vec<Arc<dyn Adapter>>) -> Self {
        let default_index = (adapters.len() == 1).then_some(0);
        Self {
            adapters,
            default_index,
        }
    }

    fn multi(&self) -> bool {
        self.adapters.len() > 1
    }

    fn names(&self) -> Vec<String> {
        self.adapters.iter().map(|a| a.name().to_string()).collect()
    }

    /// 按 `account` 参数解析目标实例（缺省回退唯一实例）。
    fn pick(&self, arguments: &Value) -> Result<Arc<dyn Adapter>, String> {
        if let Some(account) = arguments["account"].as_str().filter(|a| !a.is_empty()) {
            return self
                .adapters
                .iter()
                .find(|a| a.name() == account)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "account {account:?} 未找到；可用实例：{}",
                        self.names().join(", ")
                    )
                });
        }
        match self.default_index {
            Some(i) => Ok(self.adapters[i].clone()),
            None if self.adapters.is_empty() => Err("该人格没有可用的适配器实例".into()),
            None => Err(format!(
                "该人格有多个适配器实例，请用 account 指定：{}",
                self.names().join(", ")
            )),
        }
    }
}

/// Register adapter tools into the agent's `ToolRegistry`（该 persona 的实例集合）。
///
/// 能力过滤（泛化 S6 / 01b §D.4，不看 platform）：只对声明可出站
///（`Adapter::sends_text`）的适配器注册工具——无出站能力的适配器不是
/// 消息通道，整组不注册（send_* / get_* 一概不出现）。
///
/// - 恰好 1 个实例：不带 `account` 参数，绑定该实例；
/// - 多于 1 个：schema 增加可选 `account`（实例名），缺省时报错列出可选实例。
///
/// 返回**实际注册的工具名**，供调用方给这些工具打「所属包」标签——不再按
/// `send_` / `get_` 前缀猜。前缀猜测是「工具名前缀 = 平台语义」的隐式耦合：
/// 换个平台换套命名就漏标。
pub fn register_adapter_tools_multi(
    registry: &mut echo_agent::ToolRegistry,
    adapters: Vec<Arc<dyn Adapter>>,
    gate: Option<Arc<OutboundGate>>,
) -> Vec<(String, Option<String>)> {
    if adapters.is_empty() {
        return Vec::new();
    }
    let set = InstanceSet::new(adapters);
    // 跨实例合并工具声明（**平台语义来自适配器**，Core 不认识任何工具名）：
    // 同名工具只登记一次，参数取首个声明者的 schema（多实例是同一插件的
    // 多个进程，schema 必然一致）。
    let mut specs: Vec<echo_defs::adapter::AdapterToolSpec> = Vec::new();
    for adapter in &set.adapters {
        for spec in adapter.tool_specs() {
            if !specs.iter().any(|existing| existing.name == spec.name) {
                specs.push(spec);
            }
        }
    }
    let mut registered = Vec::new();
    for spec in specs {
        let name = spec.name.clone();
        let package = spec.package.clone();
        registry.register(Arc::new(AdapterToolBridge {
            spec,
            instances: set.clone(),
            gate: gate.clone(),
        }));
        registered.push((name, package));
    }
    registered
}

/// 把可选 `account` 参数注入工具 schema（多实例寻址）。
fn with_account(mut parameters: Value, multi: bool) -> Value {
    if !multi {
        return parameters;
    }
    if let Some(properties) = parameters
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    {
        properties.insert(
            "account".into(),
            json!({
                "type": "string",
                "description": "Adapter instance name (required when several instances exist)"
            }),
        );
    }
    parameters
}

// ---------------------------------------------------------------------------
// 管理面路由（内置 / 插件实例统一入口）
// ---------------------------------------------------------------------------

/// 经管理面调用平台自定义方法（builtin 与插件实例同一路径）。
///
/// 适配器（远程 `RemoteAdapter` 等）都实现 [`AdapterManagement`]：
/// 出站（卡片 / 语音 / 图片 / 文件）、目录与历史一律走 `invoke_custom`
/// （插件侧对应 `custom` op 的方法表）。
async fn invoke_management(
    adapter: &Arc<dyn Adapter>,
    method: &str,
    payload: Value,
) -> Result<Value, String> {
    let management = adapter.management().ok_or_else(|| {
        format!(
            "adapter {} 未提供管理面（invoke_custom 不可用）",
            adapter.name()
        )
    })?;
    management
        .invoke_custom(method, payload)
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// 通用工具桥（声明驱动）
// ---------------------------------------------------------------------------

/// 适配器声明的工具的**通用**执行壳。
///
/// 与平台无关：工具名 / 描述 / 参数全部来自
/// [`echo_defs::adapter::Adapter::tool_specs`]，执行时透传为
/// `invoke_custom(<工具名>, <参数>)`。Core 不认识任何一个工具名。
struct AdapterToolBridge {
    spec: echo_defs::adapter::AdapterToolSpec,
    /// 该 persona 的适配器实例集合（多实例经 `account` 参数寻址）。
    instances: InstanceSet,
    /// 出站闸门（None = 未启用脱敏，出站直投）。
    gate: Option<Arc<OutboundGate>>,
}

#[async_trait]
impl echo_agent::Tool for AdapterToolBridge {
    fn name(&self) -> &str {
        &self.spec.name
    }
    fn description(&self) -> &str {
        &self.spec.description
    }
    fn parameters(&self) -> Value {
        with_account(self.spec.parameters.clone(), self.instances.multi())
    }
    fn produces_visible_output(&self) -> bool {
        self.spec.visible_reply
    }
    async fn execute(&self, arguments: Value) -> Result<String, echo_agent::tool::ToolError> {
        use echo_agent::tool::ToolError;

        // 出站闸门（2026-10 安全）：所有适配器工具都跨平台边界，统一过闸
        // （敏感文本按策略阻断/替换；文件参数命中敏感目录直接拒发）。不再按
        // `send` 前缀决定——那是「工具名 = 平台语义」的隐式假设。
        let arguments = match &self.gate {
            Some(gate) => gate_outbound(&self.spec.name, arguments, gate)?,
            None => arguments,
        };

        // 多实例寻址：account 指定；单实例缺省即唯一实例。
        let adapter = self
            .instances
            .pick(&arguments)
            .map_err(ToolError::InvalidArguments)?;

        // `account` 是宿主层面的寻址参数，不进插件 payload。
        let mut payload = arguments.clone();
        if let Some(object) = payload.as_object_mut() {
            object.remove("account");
        }

        match invoke_management(&adapter, &self.spec.name, payload).await {
            Ok(outcome) => render_managed_outcome(&self.spec.name, outcome),
            Err(error) => Err(ToolError::Execution(error)),
        }
    }
}

/// 管理面返回 → 工具结果文案。
///
/// 约定：返回对象带 `success: false` 视为失败（`error` 字段给原因）；否则
/// 原样把结果 JSON 交给模型（`message_id` 等字段由插件决定）。
fn render_managed_outcome(
    tool: &str,
    outcome: Value,
) -> Result<String, echo_agent::tool::ToolError> {
    use echo_agent::tool::ToolError;
    if outcome.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(ToolError::Execution(
            outcome
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("adapter reported failure")
                .to_string(),
        ));
    }
    match outcome.get("message_id").and_then(Value::as_str) {
        Some(id) => Ok(format!("{tool}: ok, message_id={id}")),
        None => Ok(match &outcome {
            Value::Null => format!("{tool}: ok"),
            other => format!(
                "{tool}: {}",
                serde_json::to_string(other).unwrap_or_default()
            ),
        }),
    }
}
// 出站闸门（2026-10 安全）
// ---------------------------------------------------------------------------

/// `send_*` 出站检查：文件路径 → 敏感目录拒发；文本 → 命中按策略处理。
///
/// 返回（可能被脱敏后的）参数；`Block` 策略命中时返回错误（不回显任何
/// 敏感片段）。非 `send_` 名称透传（防御性；`get_*` 只读工具不在此闸门）。
fn gate_outbound(
    name: &str,
    arguments: Value,
    gate: &OutboundGate,
) -> Result<Value, echo_agent::tool::ToolError> {
    use echo_agent::tool::ToolError;
    // 1) 文件参数：落在敏感目录（配置 / 凭据目录）→ 直接拒发。
    for key in ["file_path", "file", "image"] {
        if let Some(raw) = arguments.get(key).and_then(|value| value.as_str()) {
            if let Some(path) = crate::security::sensitive_local_path(raw, &gate.protected_paths) {
                tracing::warn!(
                    tool = name,
                    path = %path.display(),
                    "adapter outbound blocked: protected path"
                );
                return Err(ToolError::Execution(format!(
                    "blocked: 文件位于受保护目录（{}），禁止外发。该目录包含本机配置/凭据，请改用其他文件。",
                    path.display()
                )));
            }
        }
    }
    // 2) 文本扫描（递归全部字符串值）：命中即按策略处理。
    let hits = scan_json_strings(&arguments, gate.redactor.as_ref());
    if hits == 0 {
        return Ok(arguments);
    }
    match gate.policy {
        OutboundPolicy::Block => {
            tracing::warn!(
                tool = name,
                hits,
                "adapter outbound blocked: sensitive content"
            );
            Err(ToolError::Execution(format!(
                "blocked: 消息内容命中敏感信息（{hits} 处，已阻止发送）。请移除密钥/令牌后重试。"
            )))
        }
        OutboundPolicy::Redact => {
            tracing::warn!(
                tool = name,
                hits,
                "adapter outbound redacted: sensitive content"
            );
            Ok(redact_json_strings(arguments, gate.redactor.as_ref()))
        }
    }
}

/// 递归统计 JSON 中全部字符串值的命中数（对象键不扫描——键是协议字段名）。
fn scan_json_strings(value: &Value, redactor: &dyn echo_defs::sanitize::Redactor) -> usize {
    match value {
        Value::String(text) => redactor.scan(text).len(),
        Value::Array(items) => items
            .iter()
            .map(|item| scan_json_strings(item, redactor))
            .sum(),
        Value::Object(map) => map
            .values()
            .map(|item| scan_json_strings(item, redactor))
            .sum(),
        _ => 0,
    }
}

/// 递归改写 JSON 中全部字符串值（对象键不动——键是协议字段名）。
fn redact_json_strings(value: Value, redactor: &dyn echo_defs::sanitize::Redactor) -> Value {
    match value {
        Value::String(text) => Value::String(redactor.redact(&text).text),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact_json_strings(item, redactor))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, item)| (key, redact_json_strings(item, redactor)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use echo_adapter::types::SendResult;
    use echo_adapter::{
        AdapterConnectionState, AdapterError, AdapterInfo, AdapterManagement, AdapterToolSpec,
    };
    use std::sync::Mutex;

    /// 记录 `invoke_custom` 调用的共享槽（适配器与管理面各持一份 Arc）。
    #[derive(Clone, Default)]
    struct Calls {
        log: Arc<Mutex<Vec<(String, Value)>>>,
    }

    /// 一个可声明工具、可记录 `invoke_custom` 调用的最小适配器。
    struct MockAdapter {
        name: String,
        tools: Vec<AdapterToolSpec>,
        calls: Calls,
        management: MockManagement,
    }

    impl MockAdapter {
        fn new(name: &str, tools: Vec<AdapterToolSpec>) -> Self {
            let calls = Calls::default();
            Self {
                name: name.to_string(),
                tools,
                management: MockManagement {
                    calls: calls.clone(),
                    reply: Arc::new(Mutex::new(None)),
                },
                calls,
            }
        }
    }

    /// 声明一个测试工具（名字随意——插件定义什么 Core 就注册什么）。
    fn spec(name: &str) -> AdapterToolSpec {
        AdapterToolSpec {
            name: name.to_string(),
            description: format!("{name} tool"),
            parameters: json!({
                "type": "object",
                "properties": { "content": {"type": "string"} },
                "required": ["content"]
            }),
            package: Some("echo-agent.adapter.mock".into()),
            visible_reply: false,
        }
    }

    struct MockManagement {
        calls: Calls,
        /// `invoke_custom` 的返回（`None` = 成功默认值）。
        reply: Arc<Mutex<Option<Value>>>,
    }

    #[async_trait]
    impl AdapterManagement for MockManagement {
        fn cached_state(&self) -> Option<Value> {
            None
        }
        async fn invoke_custom(
            &self,
            method: &str,
            payload: Value,
        ) -> Result<Value, echo_defs::adapter::AdapterError> {
            self.calls
                .log
                .lock()
                .unwrap()
                .push((method.to_string(), payload));
            Ok(self
                .reply
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| json!({"success": true, "message_id": "m1"})))
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
            true
        }
        fn status_info(&self) -> AdapterInfo {
            AdapterInfo {
                name: self.name.clone(),
                display_name: self.name.clone(),
                status: AdapterConnectionState::Connected,
                self_id: None,
                started_at: None,
                platform: "mock".into(),
                configured: true,
            }
        }
        async fn start(&self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn send_message(
            &self,
            _target: &echo_adapter::types::MessageTarget,
            _content: &str,
        ) -> Result<SendResult, AdapterError> {
            Ok(SendResult {
                message_id: Some("mock".into()),
                success: true,
                error: None,
            })
        }
        fn management(&self) -> Option<&dyn AdapterManagement> {
            Some(&self.management)
        }
        fn tool_specs(&self) -> Vec<AdapterToolSpec> {
            self.tools.clone()
        }
    }

    fn registry_with(
        adapters: Vec<Arc<dyn Adapter>>,
    ) -> (echo_agent::ToolRegistry, Vec<(String, Option<String>)>) {
        let mut registry = echo_agent::ToolRegistry::new();
        let names = register_adapter_tools_multi(&mut registry, adapters, None);
        (registry, names)
    }

    /// 工具注册**完全由适配器声明驱动**：声明什么就注册什么。
    #[tokio::test]
    async fn registers_exactly_the_declared_tools() {
        let (registry, registered) = registry_with(vec![Arc::new(MockAdapter::new(
            "mock",
            vec![spec("alpha_tool"), spec("beta_tool")],
        ))]);

        let mut names = registry.names();
        names.sort();
        assert_eq!(
            names,
            vec!["alpha_tool".to_string(), "beta_tool".to_string()],
            "只注册声明过的工具（Core 不认识任何内置工具名）"
        );
        assert_eq!(registered.len(), 2);
        assert!(registered
            .iter()
            .all(|(_, package)| package.as_deref() == Some("echo-agent.adapter.mock")));
        let definitions = registry.definitions().await;
        let alpha = definitions
            .iter()
            .find(|d| d.name == "alpha_tool")
            .expect("alpha_tool registered");
        assert_eq!(alpha.description, "alpha_tool tool");
        assert!(alpha.parameters.as_ref().unwrap()["properties"]["content"].is_object());
    }

    /// 不声明 = 不注册（「适配器不出站消息」不再是 Core 侧的能力位判断）。
    #[tokio::test]
    async fn no_declaration_registers_nothing() {
        let (registry, names) = registry_with(vec![Arc::new(MockAdapter::new("mock", Vec::new()))]);
        assert!(registry.names().is_empty(), "{:?}", registry.names());
        assert!(names.is_empty());
    }

    /// 多实例：同名工具只注册一次（schema 取首个声明者）。
    #[tokio::test]
    async fn merges_same_tool_across_instances() {
        let (registry, names) = registry_with(vec![
            Arc::new(MockAdapter::new("a", vec![spec("shared_tool")])),
            Arc::new(MockAdapter::new("b", vec![spec("shared_tool")])),
        ]);
        assert_eq!(registry.names(), vec!["shared_tool".to_string()]);
        assert_eq!(names.len(), 1);
    }

    /// 多实例：schema 自动注入 `account`；单实例不注入。
    #[tokio::test]
    async fn account_param_injected_only_for_multi_instance() {
        let (single, _) = registry_with(vec![Arc::new(MockAdapter::new("a", vec![spec("t")]))]);
        let single_defs = single.definitions().await;
        assert!(single_defs[0].parameters.as_ref().unwrap()["properties"]
            .get("account")
            .is_none());

        let (multi, _) = registry_with(vec![
            Arc::new(MockAdapter::new("a", vec![spec("t")])),
            Arc::new(MockAdapter::new("b", vec![spec("t")])),
        ]);
        let multi_defs = multi.definitions().await;
        assert!(multi_defs[0].parameters.as_ref().unwrap()["properties"]
            .get("account")
            .is_some());
    }

    /// 执行路径：`invoke_custom(<工具名>, <参数>)`，且宿主层面的 `account`
    /// **不进插件 payload**。
    #[tokio::test]
    async fn dispatches_to_invoke_custom_and_strips_account() {
        let a = Arc::new(MockAdapter::new("a", vec![spec("do_thing")]));
        let b = Arc::new(MockAdapter::new("b", vec![spec("do_thing")]));
        let (registry, _) = registry_with(vec![a.clone(), b.clone()]);

        let out = registry
            .execute("do_thing", json!({"content": "hi", "account": "b"}))
            .await
            .expect("execute ok");
        assert!(out.contains("m1"), "{out}");

        // account 命中了 b，且 payload 里没有 account。
        assert!(a.calls.log.lock().unwrap().is_empty(), "a 不应收到调用");
        let calls = b.calls.log.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "do_thing", "method = 工具名");
        assert_eq!(calls[0].1, json!({"content": "hi"}), "account 已剥离");
    }

    /// 多实例未指定 `account` → 明确报错并列出实例（不猜）。
    #[tokio::test]
    async fn multi_instance_without_account_is_an_argument_error() {
        let (registry, _) = registry_with(vec![
            Arc::new(MockAdapter::new("a", vec![spec("t")])),
            Arc::new(MockAdapter::new("b", vec![spec("t")])),
        ]);
        let err = registry
            .execute("t", json!({"content": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("account"), "{err}");
    }

    /// 插件回报 `success: false` → 工具错误（带原因）。
    #[tokio::test]
    async fn failure_reply_becomes_tool_error() {
        let adapter = MockAdapter::new("a", vec![spec("t")]);
        *adapter.management.reply.lock().unwrap() =
            Some(json!({"success": false, "error": "platform rejected"}));
        let (registry, _) = registry_with(vec![Arc::new(adapter)]);
        let err = registry
            .execute("t", json!({"content": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("platform rejected"), "{err}");
    }

    /// 闸门：敏感文本按策略阻断——对所有适配器工具生效（不再按 `send` 前缀）。
    #[tokio::test]
    async fn gate_blocks_sensitive_text_for_every_adapter_tool() {
        use echo_defs::sanitize::Redactor as _;
        let redactor = Arc::new(echo_sanitize::RegistryRedactor::new());
        redactor.register_secret("sk-secret-123", "test");
        let gate = Arc::new(OutboundGate {
            redactor,
            protected_paths: Vec::new(),
            policy: OutboundPolicy::Block,
        });

        let mut tools = echo_agent::ToolRegistry::new();
        register_adapter_tools_multi(
            &mut tools,
            vec![Arc::new(MockAdapter::new("a", vec![spec("upload")]))],
            Some(gate),
        );
        let err = tools
            .execute("upload", json!({"content": "my key is sk-secret-123"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blocked"), "{err}");
    }
}
