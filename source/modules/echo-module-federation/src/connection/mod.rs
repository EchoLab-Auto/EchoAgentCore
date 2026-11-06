//! 业务无关的连接机制：监听、拨出、认证、邀请、心跳、重连与通用应用帧。
//! 可独立用于其他 Rust 软件，但代码由 EchoAgentCore 仓库统一托管。

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
