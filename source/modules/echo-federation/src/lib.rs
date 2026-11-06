//! # echo-federation — 可平移的去中心化连接模块
//!
//! 每个节点同时可以监听与主动拨出。连接建立后双向对称，支持 Bearer 认证、
//! 握手版本检查、自连接拒绝、重复链路收敛、心跳、重连与地址学习。
//!
//! 本 crate 只依赖通用异步/序列化/WebSocket 库，不认识 Agent、工具、会话、
//! 工作区或任何具体应用。宿主通过 [`WireFrame::Application`] 定义自己的协议，
//! 通过 [`Federation::set_capability_reporter`] 上报自定义能力元数据。

pub mod error;
pub mod frame;
pub mod invite;
pub mod link;

pub use error::FederationError;
pub use frame::{Frame, NodeHello, WireFrame, PROTOCOL_VERSION};
pub use invite::{decode_invite, encode_invite, generate_token, InvitePayload, INVITE_SCHEME};
pub use link::{
    bearer_request, Federation, LinkEvent, LinkHandle, PeerConfig, PeerDiagnostics, PeerInfo,
};
