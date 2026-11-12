//! 联邦层错误类型（thiserror 具体错误；库内不依赖 anyhow）。
//!
//! 库 crate 的错误口径：用 thiserror 定义具体错误类型；`anyhow` 只出现在
//! 应用边界（`echo-agent` / `echo-agent-core`）。下游经 `?` 把
//! [`FederationError`] 自动转换为 `anyhow::Error`（它实现了
//! `std::error::Error`）。
//!
//! 错误文本尽量与迁移前的 anyhow 文本保持逐字一致（日志/测试可能匹配）；
//! `Display` 只打印本层信息，根因经 `#[source]` 链暴露（`{:?}` /
//! `Error::source`）。

use thiserror::Error;

/// 联邦层错误。
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FederationError {
    /// 底层 IO 错误（监听/读写等）。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// WebSocket 层错误（升级、帧读写；Display 透传底层文本）。
    #[error("ws error: {0}")]
    Ws(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// 帧 JSON 序列化失败。
    #[error("{0}")]
    Json(#[from] serde_json::Error),

    /// 握手超时：`HELLO_TIMEOUT` 内未收到对端首帧。
    #[error("hello timeout")]
    HelloTimeout,

    /// 握手前链路已关闭（对端未发首帧即断开）。
    #[error("link closed before hello")]
    LinkClosedBeforeHello,

    /// 握手失败（首帧类型不符、双方同时拨号让位）。
    #[error("{0}")]
    Handshake(String),

    /// 回环：对端 node_id 与本机相同（连到了自己）。
    #[error("loop detected: peer is self ({node_id})")]
    LoopDetected { node_id: String },

    /// 协议版本不兼容。
    #[error("protocol version mismatch: local {local} vs peer {peer}")]
    VersionMismatch { local: u32, peer: u32 },

    /// 出站写超时：对端不读导致 TCP 发送缓冲写满。
    #[error("federation write timed out ({secs}s) — peer not reading")]
    WriteTimeout { secs: u64 },

    /// 监听地址绑定失败。
    #[error("bind federation address {addr}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    /// Bearer 头非法（连出请求构造）。
    #[error("invalid token header")]
    InvalidTokenHeader(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// 双向拨号去重：本连接为非优选链路，主动让位。
    #[error("duplicate non-preferred link to {node_id} (peer's dial wins)")]
    NonPreferredLink { node_id: String },

    /// 出站通道关闭 = 本链路已被顶掉/移除。
    #[error("outbound channel closed (link replaced/removed)")]
    OutboundClosed,

    /// 事件出口关闭（路由层已退出）。
    #[error("event sink closed")]
    EventSinkClosed,

    /// 对端关闭连接。
    #[error("peer closed")]
    PeerClosed,

    /// 对端静默超过判死阈值。
    #[error("peer silent for >{secs}s")]
    PeerSilent { secs: u64 },

    /// 本机收到关停信号。
    #[error("shutdown")]
    Shutdown,

    /// 邀请串（`echofed://`）解析失败。
    #[error("{0}")]
    Invite(String),
}

impl From<tokio_tungstenite::tungstenite::Error> for FederationError {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::Ws(Box::new(err))
    }
}
