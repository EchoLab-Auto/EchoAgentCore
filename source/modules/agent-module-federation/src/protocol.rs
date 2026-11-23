//! 联邦状态 DTO 的兼容路径。
//!
//! 真值已上移到 `echo-protocol::federation`（它们出现在管理面线格式里）。
//! 本模块保留 `agent_module_federation::protocol::*` 与 `agent_module_federation::*`
//! 两处旧路径；联邦的**运行时**（链路、路由、聚合、端口合并）不受影响。

pub use echo_protocol::federation::{FederatedNodeInfo, FederationLinkState, FederationPeerInfo};
