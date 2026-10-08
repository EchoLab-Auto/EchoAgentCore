//! QQ-specific tools that the LLM can invoke.
//!
//! Lives in the binary crate (not echo-adapter-qq) because tools must
//! implement `echo_agent::Tool` — the adapter crate deliberately has no
//! dependency on echo-agent. These tools delegate to `QqAdapter` for actual
//! QQ API calls.

use std::sync::Arc;

use async_trait::async_trait;
use echo_adapter::traits::Adapter;
use echo_adapter::types::{ChannelType, MessageTarget};
use echo_adapter_qq::QqAdapter;
use serde_json::{json, Value};

use crate::security::OutboundPolicy;

/// QQ 出站闸门（2026-10 安全）：QQ 是消息出本的直接通道，发送前统一检查。
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

/// 该 persona 的 QQ 实例集合（多实例寻址）。
#[derive(Clone)]
struct QqInstanceSet {
    adapters: Vec<Arc<QqAdapter>>,
    /// 缺省实例（恰好一个实例时即它；多实例时 None → 必须给 account）。
    default_index: Option<usize>,
}

impl QqInstanceSet {
    fn new(adapters: Vec<Arc<QqAdapter>>) -> Self {
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
    fn pick(&self, arguments: &Value) -> Result<Arc<QqAdapter>, String> {
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
            None if self.adapters.is_empty() => Err("该人格没有可用的 QQ 实例".into()),
            None => Err(format!(
                "该人格有多个 QQ 实例，请用 account 指定：{}",
                self.names().join(", ")
            )),
        }
    }
}

/// Register QQ tools into the agent's `ToolRegistry`（该 persona 的实例集合）。
///
/// - 恰好 1 个实例：行为与旧版一致（不带 `account` 参数，绑定该实例）；
/// - 多于 1 个：schema 增加可选 `account`（实例名），缺省时报错列出可选实例。
pub fn register_qq_tools_multi(
    registry: &mut echo_agent::ToolRegistry,
    adapters: Vec<Arc<QqAdapter>>,
    gate: Option<OutboundGate>,
) {
    let set = QqInstanceSet::new(adapters);
    let multi = set.multi();
    // 闸门实例由全部 QQ 工具共享（Arc，注册闭包与运行期执行各持一份）。
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
                    "description": "QQ 实例名（多实例时指定用哪个 QQ；缺省 = 该人格唯一实例）"
                }),
            );
        }
        params
    }

    let mut register = |name: &str, description: &str, params: Value| {
        registry.register(Arc::new(QqToolWrapper {
            name: name.to_string(),
            description: description.to_string(),
            parameters: with_account(params, multi),
            instances: set.clone(),
            gate: gate.clone(),
        }));
    };

    register(
        "send_group_msg",
        "Send a text message to a QQ group. This is the only way to produce QQ-visible group output. Requires group_id and content.",
        json!({
            "type": "object",
            "properties": {
                "group_id": {"type": "integer", "description": "QQ group ID"},
                "content": {"type": "string", "description": "Message content"}
            },
            "required": ["group_id", "content"]
        }),
    );
    register(
        "send_private_msg",
        "Send a private text message to a QQ user. This is the only way to produce QQ-visible private output. Requires user_id and content.",
        json!({
            "type": "object",
            "properties": {
                "user_id": {"type": "integer", "description": "QQ user ID"},
                "content": {"type": "string", "description": "Message content"}
            },
            "required": ["user_id", "content"]
        }),
    );
    register(
        "get_group_member_info",
        "Get QQ group member information. Requires group_id and user_id.",
        json!({
            "type": "object",
            "properties": {
                "group_id": {"type": "integer", "description": "QQ group ID"},
                "user_id": {"type": "integer", "description": "QQ user ID"}
            },
            "required": ["group_id", "user_id"]
        }),
    );
    register(
        "get_group_list",
        "List all QQ groups the bot has joined. Returns group IDs and names.",
        json!({ "type": "object", "properties": {} }),
    );
    register(
        "get_friend_list",
        "List QQ friends visible under the current gate mode. Returns user IDs and nicknames.",
        json!({ "type": "object", "properties": {} }),
    );
    register(
        "get_group_msg_history",
        "Fetch a QQ group's recent message history as a readable transcript. Unlike normal inbound messages, this includes messages that did NOT mention you and messages the inbound gate filtered out — use it to catch up on group context when asked what was discussed recently, or when you need background for a reply. Gated: only groups visible under the current gate mode are readable. Requires group_id; optional count (default 50, max 500) and since_minutes (keep only messages from the last N minutes).",
        json!({
            "type": "object",
            "properties": {
                "group_id": {"type": "integer", "description": "QQ group ID"},
                "count": {"type": "integer", "description": "Max messages to fetch (default 50, max 500)"},
                "since_minutes": {"type": "integer", "description": "Only keep messages from the last N minutes"}
            },
            "required": ["group_id"]
        }),
    );
    register(
        "send_file",
        "Upload a local file to a QQ chat (group or private).          file_path is an absolute path on THIS machine (where the agent          runs) — the framework transparently bridges it to NapCat          (docker cp when possible, otherwise a local HTTP URL that NapCat          pulls from). file_name is the display name in QQ. Requires          target_type (group|private), target_id, file_path and file_name.",
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
                    "description": "QQ group ID (when target_type=group) or QQ user ID (when private)"
                },
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file on THIS machine (the agent host); the framework bridges it to NapCat automatically"
                },
                "file_name": {
                    "type": "string",
                    "description": "Display file name in QQ, e.g. report.pdf"
                }
            },
            "required": ["target_type", "target_id", "file_path", "file_name"]
        }),
    );
    register(
        "send_json_card",
        "Send a rich JSON card (OneBot json segment) to a QQ chat — this is how structured shares like a Bilibili video mini-app card are sent. The 'json' argument must be the complete card payload as a JSON string (e.g. {\"app\":\"com.tencent.miniapp_01\",\"config\":{...},\"meta\":{...}}). Requires target_type (group|private), target_id and json.",
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
                    "description": "QQ group ID (when target_type=group) or QQ user ID (when private)"
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
        "Send an image to a QQ chat (group or private). image can be a local absolute path on THIS machine (bridged to NapCat automatically), an http(s) URL, a base64:// string, or a bare file name found in NapCat's data directory. Requires target_type (group|private), target_id and image.",
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
                    "description": "QQ group ID (when target_type=group) or QQ user ID (when private)"
                },
                "image": {
                    "type": "string",
                    "description": "Image source: local absolute path on this machine, http(s) URL, base64:// data, or NapCat data dir file name"
                }
            },
            "required": ["target_type", "target_id", "image"]
        }),
    );
    register(
        "send_voice",
        "Send a QQ voice message (a native voice bubble played inline), not a downloadable file — use send_file when the user wants to save the file. file accepts: an http(s):// URL, base64:// data, a path inside the NapCat container (e.g. /app/napcat/data/x.mp3), or a path on THIS machine (the framework bridges it to NapCat automatically). A bare file name is looked up in NapCat's data directory first (where files exchanged in chat live). Common audio formats (mp3/wav/amr/silk) are converted automatically. Requires target_type (group|private), target_id and file.",
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
                    "description": "QQ group ID (when target_type=group) or QQ user ID (when private)"
                },
                "file": {
                    "type": "string",
                    "description": "Voice source: URL (http/https), base64:// data, an in-container path, a path on THIS machine, or a bare file name found in NapCat's data directory"
                }
            },
            "required": ["target_type", "target_id", "file"]
        }),
    );
}

// ---------------------------------------------------------------------------
// Internal wrapper
// ---------------------------------------------------------------------------

struct QqToolWrapper {
    name: String,
    description: String,
    parameters: Value,
    /// 该 persona 的 QQ 实例集合（多实例经 `account` 参数寻址）。
    instances: QqInstanceSet,
    /// 出站闸门（None = 未启用脱敏，行为与旧版一致）。
    gate: Option<Arc<OutboundGate>>,
}

#[async_trait]
impl echo_agent::Tool for QqToolWrapper {
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
                let group_id = arguments["group_id"]
                    .as_i64()
                    .ok_or_else(|| ToolError::InvalidArguments("group_id required".into()))?;
                let content = arguments["content"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("content required".into()))?;
                let target = MessageTarget {
                    adapter_name: adapter.name().to_string(),
                    channel: ChannelType::Group {
                        group_id: group_id.to_string(),
                    },
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
                let user_id = arguments["user_id"]
                    .as_i64()
                    .ok_or_else(|| ToolError::InvalidArguments("user_id required".into()))?;
                let content = arguments["content"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("content required".into()))?;
                let target = MessageTarget {
                    adapter_name: adapter.name().to_string(),
                    channel: ChannelType::Direct,
                    user_id: user_id.to_string(),
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
                let group_id = arguments["group_id"]
                    .as_i64()
                    .ok_or_else(|| ToolError::InvalidArguments("group_id required".into()))?;
                let user_id = arguments["user_id"]
                    .as_i64()
                    .ok_or_else(|| ToolError::InvalidArguments("user_id required".into()))?;
                match adapter.get_group_member_info(group_id, user_id).await {
                    Ok(info) => Ok(info),
                    Err(e) => Err(ToolError::Execution(e)),
                }
            }
            "get_group_list" => match adapter.get_group_list().await {
                Ok(groups) => {
                    let lines: Vec<String> = groups
                        .iter()
                        .map(|(gid, name)| format!("  {gid}: {name}"))
                        .collect();
                    if lines.is_empty() {
                        Ok("no groups joined".into())
                    } else {
                        Ok(format!("{} groups:\n{}", groups.len(), lines.join("\n")))
                    }
                }
                Err(e) => Err(ToolError::Execution(e)),
            },
            "get_friend_list" => match adapter.get_gated_friend_list().await {
                Ok(friends) => {
                    let lines: Vec<String> = friends
                        .iter()
                        .map(|(user_id, nickname)| format!("  {user_id}: {nickname}"))
                        .collect();
                    if lines.is_empty() {
                        Ok("no friends visible under the current gate".into())
                    } else {
                        Ok(format!(
                            "{} visible friends:\n{}",
                            friends.len(),
                            lines.join("\n")
                        ))
                    }
                }
                Err(e) => Err(ToolError::Execution(e)),
            },
            "get_group_msg_history" => {
                let group_id = arguments["group_id"]
                    .as_i64()
                    .ok_or_else(|| ToolError::InvalidArguments("group_id required".into()))?;
                let count = arguments["count"].as_i64().unwrap_or(50);
                let since_minutes = arguments["since_minutes"].as_u64();
                adapter
                    .get_group_msg_history(group_id, count, since_minutes)
                    .await
                    .map_err(ToolError::Execution)
            }
            "send_file" => {
                let target_type = arguments["target_type"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("target_type required".into()))?;
                let target_id = arguments["target_id"]
                    .as_i64()
                    .ok_or_else(|| ToolError::InvalidArguments("target_id required".into()))?;
                let file_path = arguments["file_path"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("file_path required".into()))?;
                let file_name = arguments["file_name"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("file_name required".into()))?;
                match target_type {
                    "group" => adapter
                        .upload_group_file(target_id, file_path, file_name)
                        .await
                        .map_err(ToolError::Execution),
                    "private" => adapter
                        .upload_private_file(target_id, file_path, file_name)
                        .await
                        .map_err(ToolError::Execution),
                    other => Err(ToolError::InvalidArguments(format!(
                        "target_type must be 'group' or 'private', got '{other}'"
                    ))),
                }
            }
            "send_json_card" => {
                let target_type = arguments["target_type"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("target_type required".into()))?;
                let target_id = arguments["target_id"]
                    .as_i64()
                    .ok_or_else(|| ToolError::InvalidArguments("target_id required".into()))?;
                let json_payload = arguments["json"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("json required".into()))?;
                // 发送前先校验是合法 JSON，避免 NapCat 侧报难懂的错。
                serde_json::from_str::<Value>(json_payload).map_err(|e| {
                    ToolError::InvalidArguments(format!("json is not valid JSON: {e}"))
                })?;
                let target = match target_type {
                    "group" => MessageTarget {
                        adapter_name: adapter.name().to_string(),
                        channel: ChannelType::Group {
                            group_id: target_id.to_string(),
                        },
                        user_id: String::new(),
                    },
                    "private" => MessageTarget {
                        adapter_name: adapter.name().to_string(),
                        channel: ChannelType::Direct,
                        user_id: target_id.to_string(),
                    },
                    other => {
                        return Err(ToolError::InvalidArguments(format!(
                            "target_type must be 'group' or 'private', got '{other}'"
                        )))
                    }
                };
                match adapter.send_json_card(&target, json_payload).await {
                    Ok(result) => {
                        if result.success {
                            Ok(format!(
                                "json card sent, message_id={}",
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
            "send_voice" => {
                let target_type = arguments["target_type"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("target_type required".into()))?;
                let target_id = arguments["target_id"]
                    .as_i64()
                    .ok_or_else(|| ToolError::InvalidArguments("target_id required".into()))?;
                let file = arguments["file"]
                    .as_str()
                    .filter(|f| !f.trim().is_empty())
                    .ok_or_else(|| ToolError::InvalidArguments("file required".into()))?;
                let target = match target_type {
                    "group" => MessageTarget {
                        adapter_name: adapter.name().to_string(),
                        channel: ChannelType::Group {
                            group_id: target_id.to_string(),
                        },
                        user_id: String::new(),
                    },
                    "private" => MessageTarget {
                        adapter_name: adapter.name().to_string(),
                        channel: ChannelType::Direct,
                        user_id: target_id.to_string(),
                    },
                    other => {
                        return Err(ToolError::InvalidArguments(format!(
                            "target_type must be 'group' or 'private', got '{other}'"
                        )))
                    }
                };
                match adapter.send_voice(&target, file).await {
                    Ok(result) => {
                        if result.success {
                            Ok(format!(
                                "voice message sent, message_id={}",
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
            "send_image" => {
                let target_type = arguments["target_type"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("target_type required".into()))?;
                let target_id = arguments["target_id"]
                    .as_i64()
                    .ok_or_else(|| ToolError::InvalidArguments("target_id required".into()))?;
                let image = arguments["image"]
                    .as_str()
                    .filter(|f| !f.trim().is_empty())
                    .ok_or_else(|| ToolError::InvalidArguments("image required".into()))?;
                let target = match target_type {
                    "group" => MessageTarget {
                        adapter_name: adapter.name().to_string(),
                        channel: ChannelType::Group {
                            group_id: target_id.to_string(),
                        },
                        user_id: String::new(),
                    },
                    "private" => MessageTarget {
                        adapter_name: adapter.name().to_string(),
                        channel: ChannelType::Direct,
                        user_id: target_id.to_string(),
                    },
                    other => {
                        return Err(ToolError::InvalidArguments(format!(
                            "target_type must be 'group' or 'private', got '{other}'"
                        )))
                    }
                };
                match adapter.send_image(&target, image).await {
                    Ok(result) => {
                        if result.success {
                            Ok(format!(
                                "image sent, message_id={}",
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
            other => Err(ToolError::Execution(format!("unknown QQ tool: {other}"))),
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
                    "QQ outbound blocked: protected path"
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
            tracing::warn!(tool = name, hits, "QQ outbound blocked: sensitive content");
            Err(ToolError::Execution(format!(
                "blocked: 消息内容命中敏感信息（{hits} 处，已阻止发送）。请移除密钥/令牌后重试。"
            )))
        }
        OutboundPolicy::Redact => {
            tracing::warn!(tool = name, hits, "QQ outbound redacted: sensitive content");
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

    #[tokio::test]
    async fn registers_gate_filtered_friend_list_tool() {
        let mut registry = echo_agent::ToolRegistry::new();
        let adapter = Arc::new(QqAdapter::new(Default::default()));

        register_qq_tools_multi(&mut registry, vec![adapter.clone()], None);

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
    async fn send_tools_are_declared_as_the_qq_output_boundary() {
        let mut registry = echo_agent::ToolRegistry::new();
        let adapter = Arc::new(QqAdapter::new(Default::default()));

        register_qq_tools_multi(&mut registry, vec![adapter.clone()], None);

        let definitions = registry.definitions().await;
        for name in ["send_private_msg", "send_group_msg"] {
            let definition = definitions
                .iter()
                .find(|definition| definition.name == name)
                .unwrap_or_else(|| panic!("missing QQ send tool: {name}"));
            assert!(
                definition.description.contains("only way"),
                "{name} must describe the explicit QQ output boundary"
            );
        }
    }

    #[tokio::test]
    async fn registers_send_file_tool() {
        let mut registry = echo_agent::ToolRegistry::new();
        let adapter = Arc::new(QqAdapter::new(Default::default()));

        register_qq_tools_multi(&mut registry, vec![adapter.clone()], None);

        let definitions = registry.definitions().await;
        let definition = definitions
            .iter()
            .find(|definition| definition.name == "send_file")
            .expect("send_file tool registered");
        assert!(definition.description.contains("file_path"));
        assert!(definition.description.contains("NapCat"));
        let params = definition.parameters.as_ref().expect("parameters present");
        assert!(params["properties"]["target_type"]["enum"][0] == "group");
        assert!(params["properties"]["target_type"]["enum"][0] == "group");
        assert!(params["properties"]["target_type"]["enum"][1] == "private");
    }

    #[tokio::test]
    async fn registers_send_json_card_tool() {
        let mut registry = echo_agent::ToolRegistry::new();
        let adapter = Arc::new(QqAdapter::new(Default::default()));

        register_qq_tools_multi(&mut registry, vec![adapter.clone()], None);

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
        let adapter = Arc::new(QqAdapter::new(Default::default()));

        register_qq_tools_multi(&mut registry, vec![adapter.clone()], None);

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
        let adapter = Arc::new(QqAdapter::new(Default::default()));

        register_qq_tools_multi(&mut registry, vec![adapter.clone()], None);

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
        let adapter = Arc::new(QqAdapter::new(Default::default()));

        register_qq_tools_multi(&mut registry, vec![adapter.clone()], None);

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
        let adapter = Arc::new(QqAdapter::new(Default::default()));
        let redactor = Arc::new(echo_sanitize::RegistryRedactor::new());
        redactor.register_secret(KEY, "api_key:test");
        let gate = OutboundGate {
            redactor,
            protected_paths: vec![std::path::PathBuf::from("/tmp/echo-gate-protected")],
            policy: OutboundPolicy::Block,
        };
        register_qq_tools_multi(&mut registry, vec![adapter], Some(gate));

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
        let adapter = Arc::new(QqAdapter::new(Default::default()));
        let redactor = Arc::new(echo_sanitize::RegistryRedactor::new());
        let gate = OutboundGate {
            redactor,
            protected_paths: vec![std::path::PathBuf::from("/tmp/echo-gate-protected")],
            policy: OutboundPolicy::Block,
        };
        register_qq_tools_multi(&mut registry, vec![adapter], Some(gate));

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
}
