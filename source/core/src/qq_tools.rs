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
use serde_json::{json, Value};

use echo_adapter_qq::QqAdapter;

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
) {
    let set = QqInstanceSet::new(adapters);
    let multi = set.multi();

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
}

// ---------------------------------------------------------------------------
// Internal wrapper

// ---------------------------------------------------------------------------
// Internal wrapper
// ---------------------------------------------------------------------------

struct QqToolWrapper {
    name: String,
    description: String,
    parameters: Value,
    /// 该 persona 的 QQ 实例集合（多实例经 `account` 参数寻址）。
    instances: QqInstanceSet,
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
            other => Err(ToolError::Execution(format!("unknown QQ tool: {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registers_gate_filtered_friend_list_tool() {
        let mut registry = echo_agent::ToolRegistry::new();
        let adapter = Arc::new(QqAdapter::new(Default::default()));

        register_qq_tools_multi(&mut registry, vec![adapter.clone()]);

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

        register_qq_tools_multi(&mut registry, vec![adapter.clone()]);

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

        register_qq_tools_multi(&mut registry, vec![adapter.clone()]);

        let definitions = registry.definitions().await;
        let definition = definitions
            .iter()
            .find(|definition| definition.name == "send_file")
            .expect("send_file tool registered");
        assert!(definition.description.contains("file_path"));
        assert!(definition.description.contains("NapCat"));
        let params = definition.parameters.as_ref().expect("parameters present");
        assert!(params["properties"]["target_type"]["enum"][0] == "group");
        assert!(params["properties"]["target_type"]["enum"][1] == "private");
    }
}
