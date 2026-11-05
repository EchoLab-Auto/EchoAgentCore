//! EchoAgent 的进程外插件领域契约。
//!
//! `echo-fabric-arch` 只搬运 `Contribution::Custom`，不认识工具、聊天平台或
//! 插件实例。这里保存宿主侧的分类名与 DTO，把领域词汇限制在适配层。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use echo_plugin_host::api::Contribution;

/// 独立远程工具的自定义贡献分类。
pub const TOOL_KIND: &str = "echo-agent.tool";
/// 聊天平台实例的自定义贡献分类。
pub const INSTANCE_KIND: &str = "echo-agent.instance";

fn default_category() -> String {
    "plugin".to_string()
}

/// 面向模型的工具声明；作为 `Custom.payload` 的 DTO。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolContribution {
    /// 工具名（也是 `Custom.name`）。
    /// 实例名（也是 `Custom.name`）。
    pub name: String,
    /// 给模型与人读的工具说明。
    pub description: String,
    #[serde(default)]
    /// 参数 JSON Schema。
    pub parameters: Value,
    #[serde(default = "default_category")]
    /// 工具展示分类。
    pub category: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 自声明超时提示（秒）。
    pub timeout_hint_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 所属包 id（面板分组）。
    pub package: Option<String>,
    #[serde(default)]
    /// 调用成功是否直接产生平台可见回复。
    pub visible_reply: bool,
}

impl ToolContribution {
    /// 编码成框架的领域中立贡献。
    pub fn into_custom(self) -> Contribution {
        Contribution::Custom {
            kind: TOOL_KIND.to_string(),
            name: self.name.clone(),
            payload: serde_json::to_value(self).expect("tool contribution is serializable"),
        }
    }
}

/// 插件实例能力位；缺省全 false，宿主按此 fail-closed。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceCapabilities {
    #[serde(default)]
    /// 支持发送文本。
    pub send_text: bool,
    #[serde(default)]
    /// 支持发送媒体。
    pub send_media: bool,
    #[serde(default)]
    /// 支持群目录查询。
    pub groups: bool,
    #[serde(default)]
    /// 支持好友目录查询。
    pub friends: bool,
    #[serde(default)]
    /// 支持门控管理。
    pub gating: bool,
    #[serde(default)]
    /// 支持登录管理。
    pub login: bool,
}

/// 插件实例声明；作为 `Custom.payload` 的 DTO。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceContribution {
    /// 实例名（也是 `Custom.name`）。
    pub name: String,
    /// 平台标识。
    pub platform: String,
    /// 展示名。
    pub display_name: String,
    #[serde(default)]
    /// 实例支持的能力位。
    pub capabilities: InstanceCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 配置 JSON Schema。
    pub config_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 宿主透传给 Panel 的 UI 描述符。
    pub ui: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 会话分组的显示标签。
    pub session_labels: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// 实例贡献给模型的工具声明。
    pub tools: Vec<ToolContribution>,
}

impl InstanceContribution {
    /// 编码成框架的领域中立贡献。实例 op 以 `name:<op>` 寻址。
    pub fn into_custom(self) -> Contribution {
        Contribution::Custom {
            kind: INSTANCE_KIND.to_string(),
            name: self.name.clone(),
            payload: serde_json::to_value(self).expect("instance contribution is serializable"),
        }
    }
}

/// 插件线协议中的通道类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelType {
    /// 私聊。
    Dm,
    /// 群聊。
    Group {
        /// 群 / 频道 id。
        group_id: String,
    },
}

/// 插件入站消息 DTO；由适配层转换为 `echo_defs::chat::IncomingMessage`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IncomingMessage {
    #[serde(alias = "adapter")]
    /// 归属插件实例名；旧字段 `adapter` 兼容。
    pub instance: String,
    /// 平台标识。
    pub platform: String,
    /// 发送者 id。
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 发送者显示名。
    pub user_name: Option<String>,
    /// 来源通道。
    pub channel: ChannelType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 群 / 频道显示名。
    pub group_name: Option<String>,
    #[serde(default)]
    /// 文本内容。
    pub content: String,
    #[serde(default)]
    /// 消息时间戳。
    pub timestamp: i64,
    #[serde(default)]
    /// 是否 @ 当前账号。
    pub at_me: bool,
    #[serde(default)]
    /// 平台附加上下文。
    pub metadata: Value,
    #[serde(default)]
    /// 入站图片引用。
    pub images: Vec<String>,
    #[serde(default)]
    /// 入站文件描述；由宿主适配为领域类型。
    pub files: Vec<Value>,
}

/// 解码一条自定义工具贡献；不属于本分类或 DTO 不合法时不认领。
pub fn tool_from_custom(contribution: &Contribution) -> Option<ToolContribution> {
    let Contribution::Custom { kind, payload, .. } = contribution else {
        return None;
    };
    (kind == TOOL_KIND)
        .then(|| serde_json::from_value(payload.clone()).ok())
        .flatten()
}

/// 解码一条自定义插件实例贡献。
pub fn instance_from_custom(contribution: &Contribution) -> Option<InstanceContribution> {
    let Contribution::Custom {
        kind,
        payload,
        name,
        ..
    } = contribution
    else {
        return None;
    };
    if kind != INSTANCE_KIND {
        return None;
    }
    let instance: InstanceContribution = serde_json::from_value(payload.clone()).ok()?;
    // name 是框架的 op 寻址入口，payload.name 是领域身份；不一致会被路由到
    // 错误的实例，必须拒绝而不是静默采用其中一个。
    (name.is_empty() || *name == instance.name).then_some(instance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_wire_contains_no_framework_domain_variants() {
        let tool = ToolContribution {
            name: "sum".into(),
            description: "sum".into(),
            parameters: serde_json::json!({"type": "object"}),
            category: default_category(),
            timeout_hint_secs: None,
            package: None,
            visible_reply: false,
        };
        let contribution = tool.clone().into_custom();
        assert_eq!(contribution.target_name(), Some("sum"));
        assert_eq!(contribution.custom_kind(), Some(TOOL_KIND));
        assert_eq!(tool_from_custom(&contribution), Some(tool));
    }

    #[test]
    fn instance_identity_must_agree_with_invoke_target() {
        let instance = InstanceContribution {
            name: "qq".into(),
            platform: "qq".into(),
            display_name: "QQ".into(),
            capabilities: InstanceCapabilities::default(),
            config_schema: None,
            ui: None,
            session_labels: None,
            tools: vec![],
        };
        let Contribution::Custom { payload, .. } = instance.clone().into_custom() else {
            unreachable!()
        };
        let mismatched = Contribution::Custom {
            kind: INSTANCE_KIND.into(),
            name: "other".into(),
            payload,
        };
        assert!(instance_from_custom(&mismatched).is_none());
        assert_eq!(
            instance_from_custom(&instance.clone().into_custom()),
            Some(instance)
        );
    }
}
