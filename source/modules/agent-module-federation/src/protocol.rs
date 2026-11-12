//! 联邦服务主导的管理协议与线上数据结构。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FederationPeerInfo {
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    #[serde(default)]
    pub token_set: bool,
    #[serde(default)]
    pub allow_tools: Vec<String>,
    #[serde(default)]
    pub require_confirm: Vec<String>,
    #[serde(default)]
    pub allow_queries: Vec<String>,
    #[serde(default)]
    pub allow_subagent: bool,
    pub link: FederationLinkState,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_online_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_advertise: Option<String>,
    /// 节点身份（Link Up 后解析到的 node_id）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// 对端握手宣告的人类可读名；只用于显示，配置名仍是策略键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    /// 该 peer 正在执行的 turn 数（活跃快照）。
    #[serde(default)]
    pub active_turns: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FederatedNodeInfo {
    pub name: String,
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub node_name: Option<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub advertise: Option<String>,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub panel_ready: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FederationLinkState {
    #[default]
    Offline,
    Online,
}
