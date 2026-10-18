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
    /// 插件实例贡献（P1，2026-10 实例插件化）：平台插件作为插件交付。
    /// 宿主把 `RemoteInstance`（`echo_defs::plugin_instance::PluginInstance` 的远程实现）
    /// 注册进 `InstanceRegistry`；动作经 `Invoke("<name>:<op>")`，入站经
    /// `HostCall` → 宿主服务 `core_api`（见 `document/design/instance-plugin.md`）。
    ///
    /// **wire tag 保持 `adapter`**：这是宿主与插件之间唯一的契约字符串，
    /// 两侧独立升级（新 Core + 已部署旧插件、旧 Core + 新插件）都依赖它不变。
    /// Rust 侧名字去掉 adapter 是内部可读性，线上一字节不动；`plugin_instance`
    /// 作为别名接受，未来若要换 tag 有过渡窗口。
    #[serde(rename = "adapter", alias = "plugin_instance")]
    PluginInstance(InstanceContribution),
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
    /// 成功调用是否即产生平台可见输出（宿主据此抑制「临时等待回复」）。
    #[serde(default)]
    pub visible_reply: bool,
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

/// 插件实例贡献（P1，2026-10 实例插件化）：宿主把 `RemoteInstance` 注册进
/// `InstanceRegistry`；`capabilities` 声明该插件实例支持的管理动作位（宿主
/// 只调用声明过的方法，未声明的调用拒绝为 `unsupported`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceContribution {
    /// 实例名（= Invoke 前缀；全局唯一）。
    pub name: String,
    /// 平台标识（会话作用域 / 显示；如 `"qq"`）。
    pub platform: String,
    /// 展示名（如 `"QQ / OneBot"`）。
    pub display_name: String,
    /// 能力位（`InstanceCapabilities`；宿主 fail-closed 校验）。
    #[serde(default)]
    pub capabilities: InstanceCapabilities,
    /// 插件配置 JSON Schema（校验 / 未来面板表单，可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    /// UI 描述符（插件声明的插件实例管理界面；Panel 经通用渲染器呈现；
    /// 结构见 `document/design/instance-plugin.md` §Panel UI 解耦，可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<Value>,
    /// 会话标签（`SessionSwitcher` 等平台分组显示；如 `{"dm":"私聊","group":"群"}`，可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_labels: Option<Value>,
    /// 该插件实例贡献给模型的工具（**平台语义在此声明，宿主不认识它们**）。
    ///
    /// 每个工具名同时是 `Invoke("<adapter>:<name>")` 的 op 名；宿主负责跨实例
    /// 合并、多实例时注入 `account` 寻址参数、把调用透传回来。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolContribution>,
}

/// 插件实例能力位（缺省全 false——宿主 fail-closed，未声明的调用拒绝）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceCapabilities {
    /// 出站文本（`send_text`）。
    #[serde(default)]
    pub send_text: bool,
    /// 出站媒体（`send_media`）。
    #[serde(default)]
    pub send_media: bool,
    /// 群目录（`get_groups`）。
    #[serde(default)]
    pub groups: bool,
    /// 好友目录（`get_friends`）。
    #[serde(default)]
    pub friends: bool,
    /// 门控管理（`custom` 的 `*_gate_*` / `update_*list` / `set_owner` 等）。
    #[serde(default)]
    pub gating: bool,
    /// 登录管理（`login_status` / `login_qrcode` 等）。
    #[serde(default)]
    pub login: bool,
}

// ── 词汇结构（与 echo-defs 词汇同构；保持本 crate 零 echo-* 依赖）─────────

/// 插件实例的目标（出站寻址）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageTarget {
    /// 通道类型：`"dm"`（私聊）/ `{"type":"group","group_id":"…"}`（群聊）。
    pub channel: ChannelType,
    /// 目标用户 id（私聊回复 / @ 提及；字符串形态，平台号码语义归插件）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// 通道类型（与 `echo_defs::chat::ChannelType` 同构）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelType {
    /// 私聊。
    Dm,
    /// 群聊。
    Group {
        /// 群 / 频道 id（字符串形态）。
        group_id: String,
    },
}

/// 入站消息（`submit_input` 载荷；与 `echo_defs::chat::IncomingMessage`
/// 同构）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IncomingMessage {
    /// 插件实例名（= 归属维度；旧线字段名 `adapter` 仍接受，兼容已部署插件）。
    #[serde(alias = "adapter")]
    pub instance: String,
    /// 平台标识（如 `"qq"`）。
    pub platform: String,
    /// 发送者 id（字符串形态）。
    pub user_id: String,
    /// 发送者显示名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    /// 通道。
    pub channel: ChannelType,
    /// 群显示名（群聊场景）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_name: Option<String>,
    /// 纯文本内容。
    #[serde(default)]
    pub content: String,
    /// 平台时间戳（Unix 秒）。
    #[serde(default)]
    pub timestamp: i64,
    /// 是否 @ 到本账号。
    #[serde(default)]
    pub at_me: bool,
    /// 平台自有元数据（透传）。
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
    /// 多模态媒体（图片 URL / `data:` URI / `/media/<id>` 引用）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// 已落盘文件附件。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<Value>,
}
