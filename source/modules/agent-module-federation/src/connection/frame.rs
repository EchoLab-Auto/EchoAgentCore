//! 业务无关的联邦线协议：控制帧 + 宿主自定义应用帧。
//!
//! `WireFrame<A>` 只负责连接生命周期和错误；`A` 由使用方定义，框架不解释其
//! 内容。应用字段可以独立演进，不必为每个新业务修改连接模块。

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Debug;

/// 可被联邦链路传输的应用帧。
pub trait Frame: Serialize + DeserializeOwned + Clone + Debug + Send + Sync + 'static {
    /// 编码完整帧。默认使用 `Application` 外壳；需要兼容既有协议的宿主可覆盖。
    fn encode_wire(frame: &WireFrame<Self>) -> Result<String, serde_json::Error> {
        serde_json::to_string(frame)
    }

    /// 解码完整帧。默认只接受本模块的 `Application` 外壳与控制帧。
    fn decode_wire(text: &str) -> Result<WireFrame<Self>, serde_json::Error> {
        serde_json::from_str(text)
    }
}
impl Frame for Value {}

/// 当前通用控制协议版本。
pub const PROTOCOL_VERSION: u32 = 1;

/// 线上帧：控制消息由本 crate 所有，应用消息透明封装宿主类型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", bound = "")]
pub enum WireFrame<A: Frame> {
    Hello(NodeHello),
    Welcome(NodeHello),
    Ping {
        sent_at_ms: i64,
    },
    Pong {
        sent_at_ms: i64,
        echo_ms: i64,
    },
    /// 宿主自定义请求、响应或事件。
    Application(A),
    /// 控制层错误；应用错误请放在 `Application` 中。
    Error {
        message: String,
    },
}

/// 节点握手信息。`caps` 是宿主自定义 JSON，不包含任何预定业务字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeHello {
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    pub protocol_version: u32,
    /// 使用方应用版本，仅用于展示与诊断。
    pub version: String,
    #[serde(default)]
    pub caps: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertise: Option<String>,
}
