//! 贡献类型：插件向宿主注册的能力（工具 / 技能 / 服务 / 事件订阅）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 一次注册的贡献。`Register` 消息携带全量集合（整体替换语义）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Contribution {
    Tool(ToolContribution),
    Skill(SkillContribution),
    Service(ServiceContribution),
    Events(EventContribution),
}

fn default_category() -> String {
    "plugin".to_string()
}

/// 面向模型的工具贡献（schema 与 `echo-defs::tool::ToolDefinition` 同口径）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolContribution {
    pub name: String,
    pub description: String,
    /// 参数 JSON Schema（与注册进 ToolRegistry 的完全一致）。
    #[serde(default)]
    pub parameters: Value,
    /// UI 分组分类（缺省 `"plugin"`）。
    #[serde(default = "default_category")]
    pub category: String,
    /// 工具自声明超时上限（秒）；宿主守卫取其与配置 base 的较大者 + 宽限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_hint_secs: Option<u64>,
    /// 所属包 id（面板按包批量勾选用；如 `echo-agent.tools.builtin`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
}

/// 技能贡献（SKILL.md 目录或内联描述）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillContribution {
    pub name: String,
    pub description: String,
    /// 技能目录（含 SKILL.md）；None = 由插件自行提供技能内容（后续扩展）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// 服务贡献：以稳定键注册的服务（供 Ctx 服务定位消费）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceContribution {
    /// 服务键（`Ctx` 命名空间；与 `ServiceKey::name()` 对应）。
    pub key: String,
    /// 服务 kind 描述（文档 / 校验用，如 `"llm"` / `"loop"`）。
    #[serde(default)]
    pub kind: String,
    /// 服务接口版本（消费端据此校验兼容）。
    #[serde(default)]
    pub version: u32,
}

/// 事件订阅：插件希望收到的宿主事件名列表（宿主按名单路由 Event 消息）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventContribution {
    #[serde(default)]
    pub subscribe: Vec<String>,
}
