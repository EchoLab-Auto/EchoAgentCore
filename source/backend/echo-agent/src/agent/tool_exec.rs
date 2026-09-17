//! Tool execution: schema validation, timeout guarding, event emission.
//!
//! Executes a tool call with preflight argument validation (schema required
//! fields), timeout guarding (respecting tool-declared `timeout_hint`), and
//! durable event logging (ToolCall/ToolResult events).

#[allow(dead_code)]
use crate::llm::ToolCall;
use crate::tool::ToolRegistry;

/// 模型可见的工具参数预检：schema 必需字段缺失时返回纠正性错误文案
/// （说清"你发了什么、应该发什么"），返回 None 表示通过预检。
pub(crate) async fn tool_arguments_error(
    tools: &ToolRegistry,
    tool_name: &str,
    raw_arguments: &str,
    args: &serde_json::Value,
) -> Option<String> {
    let schema = tools.parameters(tool_name).await?;
    invalid_tool_arguments(tool_name, raw_arguments, args, &schema)
}

/// 模型可见的工具参数预检：schema 声明的必需字段缺失时，返回纠正性错误
/// 文案（说清"你发了什么、应该发什么"）。返回 None 表示参数通过预检。
///
/// 设计动机：模型在长工具循环中可能退化出空参调用（如 bash {}），
/// 若错误反馈只是模糊的"command required"，模型不知道错在哪，会原样
/// 重试形成退化循环，烧掉整段上下文预算。
pub(crate) fn invalid_tool_arguments(
    tool_name: &str,
    raw_arguments: &str,
    args: &serde_json::Value,
    schema: &serde_json::Value,
) -> Option<String> {
    let required: Vec<&str> = schema["required"]
        .as_array()?
        .iter()
        .filter_map(|field| field.as_str())
        .collect();
    if required.is_empty() {
        return None;
    }
    // 缺失 = 键不存在或值为 null（空字符串保留给各工具自己判定，
    // 避免把 write_file content:"" 这类合法调用误判为缺参）。
    let missing: Vec<&str> = match args.as_object() {
        Some(obj) => required
            .iter()
            .filter(|field| obj.get(**field).map_or(true, |v| v.is_null()))
            .copied()
            .collect(),
        None => required.clone(),
    };
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "工具参数无效: {tool_name} 缺少必需参数 {}（你发送的参数: {}）。\
         该工具的参数 schema: {}。请按 schema 携带全部必需参数重新调用。",
        missing.join(", "),
        crate::llm::truncate(raw_arguments, 200),
        schema,
    ))
}
/// 工具超时计算：尊重工具自声明的 `timeout_hint`，外圈守卫长过 hint + 15s，
/// 硬上限 600s；用户配置的 base 不受此限。
pub(crate) async fn tool_timeout(
    tools: &ToolRegistry,
    base: std::time::Duration,
    call: &ToolCall,
) -> std::time::Duration {
    let hinted = match serde_json::from_str::<serde_json::Value>(&call.arguments) {
        Ok(args) => tools
            .timeout_hint(&call.name, &args)
            .await
            .map(|h| h + std::time::Duration::from_secs(15)),
        Err(_) => None,
    };
    hinted
        .map(|h| base.max(h.min(std::time::Duration::from_secs(600))))
        .unwrap_or(base)
}
