//! Platform adapter tools that the LLM can invoke（泛化 S6 由平台专属工具改名）。
//!
//! Lives in the binary crate: tools must implement `echo_agent::Tool` — the
//! adapter plugin deliberately has no dependency on echo-agent. 出站调用
//! 经 `Arc<dyn Adapter>` 分发（插件实例路径；平台专属动作走管理面
//! `invoke_custom`，见 [`invoke_management`]）。

use std::sync::Arc;

use async_trait::async_trait;
use echo_adapter::traits::Adapter;
use echo_adapter::types::{ChannelType, MessageTarget};
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
pub fn register_adapter_tools_multi(
    registry: &mut echo_agent::ToolRegistry,
    adapters: Vec<Arc<dyn Adapter>>,
    gate: Option<OutboundGate>,
) {
    // 能力过滤：无 `send_text` 能力 → 不注册任何工具。
    let adapters: Vec<Arc<dyn Adapter>> = adapters
        .into_iter()
        .filter(|adapter| adapter.sends_text())
        .collect();
    if adapters.is_empty() {
        return;
    }
    let set = InstanceSet::new(adapters);
    let multi = set.multi();
    // 闸门实例由全部适配器工具共享（Arc，注册闭包与运行期执行各持一份）。
    let gate = gate.map(Arc::new);
    /// 多实例时在 schema 上补 `account` 字段。
    fn with_account(params: Value, multi: bool) -> Value {
        if !multi {
            return params;
        }
        let mut params = params;
        if let Some(props) = params.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.insert(
                "account".into(),
                json!({
                    "type": "string",
                    "description": "适配器实例名（多实例时指定用哪个实例；缺省 = 该人格唯一实例）"
                }),
            );
        }
        params
    }

    let mut register = |name: &str, description: &str, params: Value| {
        registry.register(Arc::new(AdapterToolWrapper {
            name: name.to_string(),
            description: description.to_string(),
            parameters: with_account(params, multi),
            instances: set.clone(),
            gate: gate.clone(),
        }));
    };

    register(
        "send_group_msg",
        "Send a text message to a group. This is the only way to produce group-visible output. Requires group_id and content.",
        json!({
            "type": "object",
            "properties": {
                "group_id": {"type": "integer", "description": "group ID"},
                "content": {"type": "string", "description": "Message content"}
            },
            "required": ["group_id", "content"]
        }),
    );
    register(
        "send_private_msg",
        "Send a private text message to a user. This is the only way to produce private-visible output. Requires user_id and content.",
        json!({
            "type": "object",
            "properties": {
                "user_id": {"type": "integer", "description": "user ID"},
                "content": {"type": "string", "description": "Message content"}
            },
            "required": ["user_id", "content"]
        }),
    );
    register(
        "get_group_member_info",
        "Get group member information. Requires group_id and user_id.",
        json!({
            "type": "object",
            "properties": {
                "group_id": {"type": "integer", "description": "group ID"},
                "user_id": {"type": "integer", "description": "user ID"}
            },
            "required": ["group_id", "user_id"]
        }),
    );
    register(
        "get_group_list",
        "List all groups the bot has joined. Returns group IDs and names.",
        json!({ "type": "object", "properties": {} }),
    );
    register(
        "get_friend_list",
        "List friends visible under the current gate mode. Returns user IDs and nicknames.",
        json!({ "type": "object", "properties": {} }),
    );
    register(
        "get_group_msg_history",
        "Fetch a group's recent message history as a readable transcript. Unlike normal inbound messages, this includes messages that did NOT mention you and messages the inbound gate filtered out — use it to catch up on group context when asked what was discussed recently, or when you need background for a reply. Gated: only groups visible under the current gate mode are readable. Requires group_id; optional count (default 50, max 500) and since_minutes (keep only messages from the last N minutes).",
        json!({
            "type": "object",
            "properties": {
                "group_id": {"type": "integer", "description": "group ID"},
                "count": {"type": "integer", "description": "Max messages to fetch (default 50, max 500)"},
                "since_minutes": {"type": "integer", "description": "Only keep messages from the last N minutes"}
            },
            "required": ["group_id"]
        }),
    );
    register(
        "send_file",
        "Upload a local file to a chat (group or private).          file_path is an absolute path on THIS machine (where the agent          runs) — the framework transparently bridges it to the adapter          runtime. file_name is the display name in the chat. Requires          target_type (group|private), target_id, file_path and file_name.",
        json!({
            "type": "object",
            "properties": {
                "target_type": {
                    "type": "string",
                    "enum": ["group", "private"],
                    "description": "Where to send: 'group' uploads to a group, 'private' to a user"
                },
                "target_id": {
                    "type": "integer",
                    "description": "group ID (when target_type=group) or user ID (when private)"
                },
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file on THIS machine (the agent host); the framework bridges it to the adapter automatically"
                },
                "file_name": {
                    "type": "string",
                    "description": "Display file name in the chat, e.g. report.pdf"
                }
            },
            "required": ["target_type", "target_id", "file_path", "file_name"]
        }),
    );
    register(
        "send_json_card",
        "Send a rich JSON card (platform json segment) to a chat — this is how structured shares like video mini-app cards are sent. The 'json' argument must be the complete card payload as a JSON string (e.g. {\"app\":\"com.tencent.miniapp_01\",\"config\":{...},\"meta\":{...}}). Requires target_type (group|private), target_id and json.",
        json!({
            "type": "object",
            "properties": {
                "target_type": {
                    "type": "string",
                    "enum": ["group", "private"],
                    "description": "Where to send: 'group' or 'private'"
                },
                "target_id": {
                    "type": "integer",
                    "description": "group ID (when target_type=group) or user ID (when private)"
                },
                "json": {
                    "type": "string",
                    "description": "Complete card payload as a JSON string"
                }
            },
            "required": ["target_type", "target_id", "json"]
        }),
    );
    register(
        "send_image",
        "Send an image to a chat (group or private). image can be a local absolute path on THIS machine (bridged to the adapter automatically), an http(s) URL, a base64:// string, or a bare file name found in the adapter's data directory. Requires target_type (group|private), target_id and image.",
        json!({
            "type": "object",
            "properties": {
                "target_type": {
                    "type": "string",
                    "enum": ["group", "private"],
                    "description": "Where to send: 'group' or 'private'"
                },
                "target_id": {
                    "type": "integer",
                    "description": "group ID (when target_type=group) or user ID (when private)"
                },
                "image": {
                    "type": "string",
                    "description": "Image source: local absolute path on this machine, http(s) URL, base64:// data, or a file name in the adapter's data directory"
                }
            },
            "required": ["target_type", "target_id", "image"]
        }),
    );
    register(
        "send_voice",
        "Send a voice message (a native voice bubble played inline), not a downloadable file — use send_file when the user wants to save the file. file accepts: an http(s):// URL, base64:// data, a path inside the adapter runtime (e.g. /data/x.mp3), or a path on THIS machine (the framework bridges it to the adapter automatically). A bare file name is looked up in the adapter's data directory first (where files exchanged in chat live). Common audio formats (mp3/wav/amr/silk) are converted automatically. Requires target_type (group|private), target_id and file.",
        json!({
            "type": "object",
            "properties": {
                "target_type": {
                    "type": "string",
                    "enum": ["group", "private"],
                    "description": "Where to send: 'group' or 'private'"
                },
                "target_id": {
                    "type": "integer",
                    "description": "group ID (when target_type=group) or user ID (when private)"
                },
                "file": {
                    "type": "string",
                    "description": "Voice source: URL (http/https), base64:// data, an in-container path, a path on THIS machine, or a bare file name found in the adapter's data directory"
                }
            },
            "required": ["target_type", "target_id", "file"]
        }),
    );
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

/// 工具 id 参数宽容解析（W2.4 去号码化，非 breaking）：模型可能传 JSON
/// number 或 string，统一归一为 `String`（number → `to_string()`）。
/// 空串 / 缺省 / 其它形态视为参数缺失（与旧 `as_i64` 语义一致地报错）。
fn arg_id(arguments: &Value, key: &str) -> Result<String, echo_agent::tool::ToolError> {
    use echo_agent::tool::ToolError;
    match arguments.get(key) {
        Some(Value::Number(number)) => Ok(number.to_string()),
        Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
        _ => Err(ToolError::InvalidArguments(format!("{key} required"))),
    }
}

/// 工具参数（`target_type` / `target_id`）→ 管理面目标 JSON。
/// `target_id` 已由 [`arg_id`] 归一为字符串（W2.4 线上一律字符串）。
fn management_target(
    target_type: &str,
    target_id: &str,
) -> Result<Value, echo_agent::tool::ToolError> {
    use echo_agent::tool::ToolError;
    match target_type {
        "group" => Ok(json!({
            "channel": {"type": "group", "group_id": target_id},
            "user_id": "",
        })),
        "private" => Ok(json!({
            "channel": {"type": "dm"},
            "user_id": target_id,
        })),
        other => Err(ToolError::InvalidArguments(format!(
            "target_type must be 'group' or 'private', got '{other}'"
        ))),
    }
}

/// 管理面 `send_*` 返回（`{message_id?, success, error?}`）→ 工具结果文案。
fn render_managed_send(
    label: &str,
    outcome: Result<Value, String>,
) -> Result<String, echo_agent::tool::ToolError> {
    use echo_agent::tool::ToolError;
    match outcome {
        Ok(out) => {
            if out["success"].as_bool().unwrap_or(true) {
                Ok(format!(
                    "{label} sent, message_id={}",
                    out["message_id"].as_str().unwrap_or("?")
                ))
            } else {
                Err(ToolError::Execution(
                    out["error"].as_str().unwrap_or("unknown error").to_string(),
                ))
            }
        }
        Err(e) => Err(ToolError::Execution(e)),
    }
}

// ---------------------------------------------------------------------------
// Internal wrapper
// ---------------------------------------------------------------------------

struct AdapterToolWrapper {
    name: String,
    description: String,
    parameters: Value,
    /// 该 persona 的适配器实例集合（多实例经 `account` 参数寻址）。
    instances: InstanceSet,
    /// 出站闸门（None = 未启用脱敏，出站直投）。
    gate: Option<Arc<OutboundGate>>,
}

#[async_trait]
impl echo_agent::Tool for AdapterToolWrapper {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        self.parameters.clone()
    }
    async fn execute(&self, arguments: Value) -> Result<String, echo_agent::tool::ToolError> {
        use echo_agent::tool::ToolError;

        // 出站闸门（2026-10 安全）：send_* 先过敏感信息检查（文本命中
        // 按策略阻断/替换；文件参数命中敏感目录直接拒发）。
        let arguments = match &self.gate {
            Some(gate) => gate_outbound(&self.name, arguments, gate)?,
            None => arguments,
        };

        // 多实例寻址：account 指定；单实例缺省即唯一实例。
        let adapter = self
            .instances
            .pick(&arguments)
            .map_err(ToolError::InvalidArguments)?;
        match self.name.as_str() {
            "send_group_msg" => {
                let group_id = arg_id(&arguments, "group_id")?;
                let content = arguments["content"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("content required".into()))?;
                let target = MessageTarget {
                    adapter_name: adapter.name().to_string(),
                    channel: ChannelType::Group { group_id },
                    user_id: String::new(),
                };
                match adapter.send_message(&target, content).await {
                    Ok(result) => {
                        if result.success {
                            Ok(format!(
                                "group message sent, message_id={}",
                                result.message_id.as_deref().unwrap_or("?")
                            ))
                        } else {
                            Err(ToolError::Execution(
                                result.error.unwrap_or_else(|| "unknown error".into()),
                            ))
                        }
                    }
                    Err(e) => Err(ToolError::Execution(e.to_string())),
                }
            }
            "send_private_msg" => {
                let user_id = arg_id(&arguments, "user_id")?;
                let content = arguments["content"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("content required".into()))?;
                let target = MessageTarget {
                    adapter_name: adapter.name().to_string(),
                    channel: ChannelType::Direct,
                    user_id,
                };
                match adapter.send_message(&target, content).await {
                    Ok(result) => {
                        if result.success {
                            Ok(format!(
                                "private message sent, message_id={}",
                                result.message_id.as_deref().unwrap_or("?")
                            ))
                        } else {
                            Err(ToolError::Execution(
                                result.error.unwrap_or_else(|| "unknown error".into()),
                            ))
                        }
                    }
                    Err(e) => Err(ToolError::Execution(e.to_string())),
                }
            }
            "get_group_member_info" => {
                let group_id = arg_id(&arguments, "group_id")?;
                let user_id = arg_id(&arguments, "user_id")?;
                match invoke_management(
                    &adapter,
                    "get_group_member_info",
                    json!({"group_id": group_id, "user_id": user_id}),
                )
                .await
                {
                    Ok(out) => Ok(out
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| out.to_string())),
                    Err(e) => Err(ToolError::Execution(e)),
                }
            }
            "get_group_list" => {
                match invoke_management(&adapter, "get_group_list", json!({})).await {
                    Ok(out) => {
                        let lines: Vec<String> = out["groups"]
                            .as_array()
                            .map(|groups| {
                                groups
                                    .iter()
                                    .map(|group| {
                                        format!(
                                            "  {}: {}",
                                            group["id"].as_str().unwrap_or("?"),
                                            group["name"].as_str().unwrap_or("?")
                                        )
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if lines.is_empty() {
                            Ok("no groups joined".into())
                        } else {
                            Ok(format!("{} groups:\n{}", lines.len(), lines.join("\n")))
                        }
                    }
                    Err(e) => Err(ToolError::Execution(e)),
                }
            }
            "get_friend_list" => {
                match invoke_management(&adapter, "get_gated_friend_list", json!({})).await {
                    Ok(out) => {
                        let lines: Vec<String> = out["friends"]
                            .as_array()
                            .map(|friends| {
                                friends
                                    .iter()
                                    .map(|friend| {
                                        format!(
                                            "  {}: {}",
                                            friend["id"].as_str().unwrap_or("?"),
                                            friend["name"].as_str().unwrap_or("?")
                                        )
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if lines.is_empty() {
                            Ok("no friends visible under the current gate".into())
                        } else {
                            Ok(format!(
                                "{} visible friends:\n{}",
                                lines.len(),
                                lines.join("\n")
                            ))
                        }
                    }
                    Err(e) => Err(ToolError::Execution(e)),
                }
            }
            "get_group_msg_history" => {
                let group_id = arg_id(&arguments, "group_id")?;
                let count = arguments["count"].as_i64().unwrap_or(50);
                let mut payload = json!({"group_id": group_id, "count": count});
                if let Some(since_minutes) = arguments["since_minutes"].as_u64() {
                    payload["since_minutes"] = json!(since_minutes);
                }
                match invoke_management(&adapter, "get_group_msg_history", payload).await {
                    Ok(out) => Ok(out
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| out.to_string())),
                    Err(e) => Err(ToolError::Execution(e)),
                }
            }
            "send_file" => {
                let target_type = arguments["target_type"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("target_type required".into()))?;
                let target_id = arg_id(&arguments, "target_id")?;
                let file_path = arguments["file_path"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("file_path required".into()))?;
                let file_name = arguments["file_name"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("file_name required".into()))?;
                let target = management_target(target_type, &target_id)?;
                match invoke_management(
                    &adapter,
                    "send_file",
                    json!({"target": target, "file_path": file_path, "file_name": file_name}),
                )
                .await
                {
                    Ok(out) => Ok(out
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| out.to_string())),
                    Err(e) => Err(ToolError::Execution(e)),
                }
            }
            "send_json_card" => {
                let target_type = arguments["target_type"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("target_type required".into()))?;
                let target_id = arg_id(&arguments, "target_id")?;
                let json_payload = arguments["json"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("json required".into()))?;
                // 发送前先校验是合法 JSON，避免适配器侧报难懂的错。
                serde_json::from_str::<Value>(json_payload).map_err(|e| {
                    ToolError::InvalidArguments(format!("json is not valid JSON: {e}"))
                })?;
                let target = management_target(target_type, &target_id)?;
                render_managed_send(
                    "json card",
                    invoke_management(
                        &adapter,
                        "send_json_card",
                        json!({"target": target, "json": json_payload}),
                    )
                    .await,
                )
            }
            "send_voice" => {
                let target_type = arguments["target_type"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("target_type required".into()))?;
                let target_id = arg_id(&arguments, "target_id")?;
                let file = arguments["file"]
                    .as_str()
                    .filter(|f| !f.trim().is_empty())
                    .ok_or_else(|| ToolError::InvalidArguments("file required".into()))?;
                let target = management_target(target_type, &target_id)?;
                render_managed_send(
                    "voice message",
                    invoke_management(
                        &adapter,
                        "send_voice",
                        json!({"target": target, "file": file}),
                    )
                    .await,
                )
            }
            "send_image" => {
                let target_type = arguments["target_type"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("target_type required".into()))?;
                let target_id = arg_id(&arguments, "target_id")?;
                let image = arguments["image"]
                    .as_str()
                    .filter(|f| !f.trim().is_empty())
                    .ok_or_else(|| ToolError::InvalidArguments("image required".into()))?;
                let target = management_target(target_type, &target_id)?;
                render_managed_send(
                    "image",
                    invoke_management(
                        &adapter,
                        "send_image",
                        json!({"target": target, "image": image}),
                    )
                    .await,
                )
            }
            other => Err(ToolError::Execution(format!(
                "unknown adapter tool: {other}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
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
    if !name.starts_with("send") {
        return Ok(arguments);
    }
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

    use echo_adapter::types::{AdapterEvent, SendResult};
    use echo_adapter::{AdapterConnectionState, AdapterError, AdapterInfo, AdapterManagement};

    /// 测试用最小适配器：工具注册 / 闸门用例不触达真实平台路径。
    /// `send_capable` 对应 `Adapter::sends_text` 能力位（泛化 S6）。
    struct MockAdapter {
        send_capable: bool,
    }

    #[async_trait]
    impl Adapter for MockAdapter {
        fn name(&self) -> &str {
            "mock"
        }
        fn display_name(&self) -> &str {
            "mock"
        }
        fn platform(&self) -> &str {
            "mock"
        }
        fn sends_text(&self) -> bool {
            self.send_capable
        }
        fn is_configured(&self) -> bool {
            true
        }
        fn status_info(&self) -> AdapterInfo {
            AdapterInfo {
                name: "mock".into(),
                display_name: "mock".into(),
                status: AdapterConnectionState::Stopped,
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
            _target: &MessageTarget,
            _content: &str,
        ) -> Result<SendResult, AdapterError> {
            Ok(SendResult {
                message_id: Some("mock".into()),
                success: true,
                error: None,
            })
        }
        fn subscribe(&self, _tx: tokio::sync::mpsc::UnboundedSender<AdapterEvent>) {}
        fn management(&self) -> Option<&dyn AdapterManagement> {
            None
        }
    }

    /// 单实例 mock 集合（测试注册入口；可出站）。
    fn mock_instances() -> Vec<Arc<dyn Adapter>> {
        vec![Arc::new(MockAdapter { send_capable: true })]
    }

    /// 泛化 S6（01b §D.4）：无 send 能力（`sends_text=false`）的适配器
    /// 不注册 send_* 工具（整组按能力过滤：适配器不出现在实例集，一个
    /// 工具都不注册）。
    #[tokio::test]
    async fn no_send_capability_registers_no_send_tools() {
        let mut registry = echo_agent::ToolRegistry::new();

        register_adapter_tools_multi(
            &mut registry,
            vec![Arc::new(MockAdapter {
                send_capable: false,
            })],
            None,
        );

        let names = registry.names();
        assert!(
            !names.iter().any(|name| name.starts_with("send_")),
            "no send_* tools for a non-sending adapter: {names:?}"
        );
        assert!(
            names.is_empty(),
            "non-sending adapter must not register any adapter tool: {names:?}"
        );
    }

    #[tokio::test]
    async fn registers_gate_filtered_friend_list_tool() {
        let mut registry = echo_agent::ToolRegistry::new();

        register_adapter_tools_multi(&mut registry, mock_instances(), None);

        assert!(registry
            .names()
            .iter()
            .any(|name| name == "get_friend_list"));
        let definitions = registry.definitions().await;
        let definition = definitions
            .iter()
            .find(|definition| definition.name == "get_friend_list")
            .expect("friend list tool definition");
        assert!(definition.description.contains("gate"));
    }

    #[tokio::test]
    async fn send_tools_are_declared_as_the_output_boundary() {
        let mut registry = echo_agent::ToolRegistry::new();

        register_adapter_tools_multi(&mut registry, mock_instances(), None);

        let definitions = registry.definitions().await;
        for name in ["send_private_msg", "send_group_msg"] {
            let definition = definitions
                .iter()
                .find(|definition| definition.name == name)
                .unwrap_or_else(|| panic!("missing send tool: {name}"));
            assert!(
                definition.description.contains("only way"),
                "{name} must describe the explicit output boundary"
            );
        }
    }

    #[tokio::test]
    async fn registers_send_file_tool() {
        let mut registry = echo_agent::ToolRegistry::new();

        register_adapter_tools_multi(&mut registry, mock_instances(), None);

        let definitions = registry.definitions().await;
        let definition = definitions
            .iter()
            .find(|definition| definition.name == "send_file")
            .expect("send_file tool registered");
        assert!(definition.description.contains("file_path"));
        assert!(definition.description.contains("bridges"));
        let params = definition.parameters.as_ref().expect("parameters present");
        assert!(params["properties"]["target_type"]["enum"][0] == "group");
        assert!(params["properties"]["target_type"]["enum"][0] == "group");
        assert!(params["properties"]["target_type"]["enum"][1] == "private");
    }

    #[tokio::test]
    async fn registers_send_json_card_tool() {
        let mut registry = echo_agent::ToolRegistry::new();

        register_adapter_tools_multi(&mut registry, mock_instances(), None);

        let definitions = registry.definitions().await;
        let definition = definitions
            .iter()
            .find(|definition| definition.name == "send_json_card")
            .expect("send_json_card tool registered");
        assert!(definition.description.contains("json"));
        let params = definition.parameters.as_ref().expect("parameters present");
        assert!(params["properties"]["target_type"]["enum"][0] == "group");
        assert!(params["properties"]["target_type"]["enum"][1] == "private");
        let required = params["required"].as_array().expect("required list");
        assert!(required.iter().any(|v| v == "json"));
    }

    #[tokio::test]
    async fn registers_send_voice_tool() {
        let mut registry = echo_agent::ToolRegistry::new();

        register_adapter_tools_multi(&mut registry, mock_instances(), None);

        let definitions = registry.definitions().await;
        let definition = definitions
            .iter()
            .find(|definition| definition.name == "send_voice")
            .expect("send_voice tool registered");
        // 语义边界：语音气泡（非文件）且注明与 send_file 的分工。
        assert!(definition.description.contains("voice message"));
        assert!(definition.description.contains("send_file"));
        let params = definition.parameters.as_ref().expect("parameters present");
        assert!(params["properties"]["target_type"]["enum"][0] == "group");
        assert!(params["properties"]["target_type"]["enum"][1] == "private");
        assert!(params["properties"]["file"]["type"] == "string");
        let required = params["required"].as_array().expect("required list");
        for field in ["target_type", "target_id", "file"] {
            assert!(
                required.iter().any(|v| v == field),
                "missing required: {field}"
            );
        }
    }

    #[tokio::test]
    async fn registers_get_group_msg_history_tool() {
        let mut registry = echo_agent::ToolRegistry::new();

        register_adapter_tools_multi(&mut registry, mock_instances(), None);

        let definitions = registry.definitions().await;
        let definition = definitions
            .iter()
            .find(|definition| definition.name == "get_group_msg_history")
            .expect("get_group_msg_history tool registered");
        // 语义：历史包含未 @ 的消息；受群门控约束。
        assert!(definition.description.contains("did NOT mention"));
        assert!(definition.description.contains("Gated"));
        let params = definition.parameters.as_ref().expect("parameters present");
        assert!(params["properties"]["group_id"]["type"] == "integer");
        assert!(params["properties"]["count"]["type"] == "integer");
        assert!(params["properties"]["since_minutes"]["type"] == "integer");
        let required = params["required"].as_array().expect("required list");
        assert_eq!(required.len(), 1);
        assert!(required.iter().any(|v| v == "group_id"));
    }

    #[tokio::test]
    async fn registers_send_image_tool() {
        let mut registry = echo_agent::ToolRegistry::new();

        register_adapter_tools_multi(&mut registry, mock_instances(), None);

        let definitions = registry.definitions().await;
        let definition = definitions
            .iter()
            .find(|definition| definition.name == "send_image")
            .expect("send_image tool registered");
        assert!(definition.description.contains("image"));
        let params = definition.parameters.as_ref().expect("parameters present");
        assert!(params["properties"]["target_type"]["enum"][0] == "group");
        assert!(params["properties"]["target_type"]["enum"][1] == "private");
        assert!(params["properties"]["image"]["type"] == "string");
        let required = params["required"].as_array().expect("required list");
        for field in ["target_type", "target_id", "image"] {
            assert!(
                required.iter().any(|v| v == field),
                "missing required: {field}"
            );
        }
    }

    /// 出站闸门：命中注册密钥 → 阻断（错误不回显敏感片段）。
    #[tokio::test]
    async fn outbound_gate_blocks_secret_content() {
        use echo_defs::sanitize::Redactor;
        const KEY: &str = "sk-gateblockgateblock0001";
        let mut registry = echo_agent::ToolRegistry::new();
        let redactor = Arc::new(echo_sanitize::RegistryRedactor::new());
        redactor.register_secret(KEY, "api_key:test");
        let gate = OutboundGate {
            redactor,
            protected_paths: vec![std::path::PathBuf::from("/tmp/echo-gate-protected")],
            policy: OutboundPolicy::Block,
        };
        register_adapter_tools_multi(&mut registry, mock_instances(), Some(gate));

        let error = registry
            .execute(
                "send_group_msg",
                json!({"group_id": 1, "content": format!("here you go {KEY}")}),
            )
            .await
            .expect_err("send must be blocked");
        let text = error.to_string();
        assert!(text.contains("blocked"), "got: {text}");
        assert!(!text.contains(KEY), "error must not echo the secret");
    }

    /// 出站闸门：文件参数落在敏感目录 → 拒发（不触达 adapter）。
    #[tokio::test]
    async fn outbound_gate_blocks_protected_path() {
        let mut registry = echo_agent::ToolRegistry::new();
        let redactor = Arc::new(echo_sanitize::RegistryRedactor::new());
        let gate = OutboundGate {
            redactor,
            protected_paths: vec![std::path::PathBuf::from("/tmp/echo-gate-protected")],
            policy: OutboundPolicy::Block,
        };
        register_adapter_tools_multi(&mut registry, mock_instances(), Some(gate));

        let error = registry
            .execute(
                "send_file",
                json!({
                    "target_type": "group",
                    "target_id": 1,
                    "file_path": "/tmp/echo-gate-protected/core.toml",
                    "file_name": "core.toml"
                }),
            )
            .await
            .expect_err("protected file must be blocked");
        assert!(error.to_string().contains("blocked"));
    }

    /// redact 策略：内容被替换后放行（闸门函数级验证，不依赖网络）。
    #[test]
    fn outbound_gate_redact_policy_rewrites_strings() {
        use echo_defs::sanitize::Redactor;
        const KEY: &str = "sk-gateredactgateredact01";
        let redactor = Arc::new(echo_sanitize::RegistryRedactor::new());
        redactor.register_secret(KEY, "api_key:test");
        let gate = OutboundGate {
            redactor,
            protected_paths: vec![],
            policy: OutboundPolicy::Redact,
        };
        let out = gate_outbound(
            "send_group_msg",
            json!({
                "group_id": 1,
                "content": format!("key {KEY} end"),
                "nested": [format!("x {KEY}")]
            }),
            &gate,
        )
        .expect("redact mode must pass");
        let rendered = out.to_string();
        assert!(!rendered.contains(KEY));
        assert!(rendered.contains("【已隐藏:api_key:test】"));
    }

    /// 无命中 / 非 send 工具：原样透传。
    #[test]
    fn outbound_gate_passthrough_without_hits() {
        let redactor = Arc::new(echo_sanitize::RegistryRedactor::new());
        let gate = OutboundGate {
            redactor,
            protected_paths: vec![],
            policy: OutboundPolicy::Block,
        };
        let args = json!({"group_id": 1, "content": "hello world"});
        assert_eq!(
            gate_outbound("send_group_msg", args.clone(), &gate).unwrap(),
            args
        );
        let get = json!({"group_id": 1});
        assert_eq!(
            gate_outbound("get_group_list", get.clone(), &gate).unwrap(),
            get
        );
    }

    /// W2.4：工具 id 参数宽容解析——number / string 均可（number → String）。
    #[test]
    fn arg_id_accepts_number_and_string() {
        assert_eq!(arg_id(&json!({"id": 123}), "id").unwrap(), "123");
        assert_eq!(arg_id(&json!({"id": "123"}), "id").unwrap(), "123");
        assert!(arg_id(&json!({}), "id").is_err(), "缺省 = 参数缺失");
        assert!(arg_id(&json!({"id": null}), "id").is_err());
        assert!(arg_id(&json!({"id": ""}), "id").is_err(), "空串 = 参数缺失");
        assert!(arg_id(&json!({"id": {"x": 1}}), "id").is_err());
    }

    /// W2.4：管理面目标 JSON 的 id 一律字符串（与线协议去号码化一致）。
    #[test]
    fn management_target_uses_string_ids() {
        let group = management_target("group", "123").unwrap();
        assert_eq!(group["channel"]["group_id"], serde_json::json!("123"));
        assert_eq!(group["user_id"], serde_json::json!(""));
        let dm = management_target("private", "456").unwrap();
        assert_eq!(dm["channel"]["type"], serde_json::json!("dm"));
        assert_eq!(dm["user_id"], serde_json::json!("456"));
        assert!(management_target("channel", "1").is_err());
    }
}
