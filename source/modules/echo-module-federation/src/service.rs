//! 联邦服务的命令与事件协议。

use crate::protocol::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FederationCommand {
    RequestFederationStatus,
    RequestFederationDirectory,
    SaveFederationPeer {
        peer: FederationPeerInfo,
    },
    DeleteFederationPeer {
        name: String,
    },
    RequestFederationInvite,
    MigrateSession {
        session_id: String,
        #[serde(default)]
        team_id: Option<String>,
        target_peer: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FederationEvent {
    FederationStatus {
        enabled: bool,
        #[serde(default)]
        node_id: String,
        #[serde(default)]
        node_name: Option<String>,
        #[serde(default)]
        listen: String,
        #[serde(default)]
        peers: Vec<FederationPeerInfo>,
        #[serde(default)]
        panel_ready: bool,
        #[serde(default)]
        panel_access: String,
    },
    FederationDirectory {
        #[serde(default)]
        node_id: String,
        #[serde(default)]
        node_name: Option<String>,
        #[serde(default)]
        listen: String,
        #[serde(default)]
        panel_ready: bool,
        #[serde(default)]
        panel_access: String,
        #[serde(default)]
        nodes: Vec<FederatedNodeInfo>,
    },
    SessionMigrated {
        session_id: String,
        target_peer: String,
        #[serde(default)]
        new_session_id: Option<String>,
        message: String,
        #[serde(default)]
        success: bool,
    },
    FederationInvite {
        invite: String,
    },
    Error {
        session_id: Option<String>,
        message: String,
    },
}
