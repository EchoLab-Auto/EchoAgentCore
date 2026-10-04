//! EchoAgentCore 联邦层（federation Phase 1）：Core↔Core 对等链路。
//!
//! - [`frame`]：`FedFrame` 线协议（工具调用/委派/查询帧 + 握手能力协商）
//! - [`link`]：连接管理（监听/连出、Hello/Welcome 握手、per-peer Bearer
//!   认证、心跳保活、断线重连、回环拒绝）
//!
//! Phase 1 只提供链路；工具路由（Phase 2）与远程委派（Phase 3）消费
//! [`LinkEvent`] 与 [`Federation::send_to`]。

pub mod frame;
pub mod invite;
pub mod link;

pub use frame::{
    call_origin, new_call_id, FedError, FedFrame, InvokeRequest, InvokeResult, InvokeVerdict,
    NodeCaps, NodeHello, OutputStream, QueryKind, QueryRequest, QueryResultFrame,
    SessionImportAckFrame, SessionImportFrame, SessionImportResultFrame, SubagentEventFrame,
    SubagentSpawnRequest,
    SubagentStatus, PROTOCOL_VERSION,
};
pub use invite::{decode_invite, encode_invite, generate_token, InvitePayload, INVITE_SCHEME};
pub use link::{error_frame, Federation, LinkEvent, LinkHandle, PeerConfig, PeerInfo};
