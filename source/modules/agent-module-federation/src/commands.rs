//! 联邦命令域：5 条 `*Federation*` / `MigrateSession` 命令。
//!
//! ## 为什么这几条归本模块
//!
//! 它们是**联邦的管理面**：保存对端、生成邀请串、查询链路状态、迁移会话。
//! 此前它们在宿主 `apply_command` 里是一段"问 `ports.command_handler`，
//! 没有就报「联邦未启用」"的内联分支——命令归属关系写在宿主里，而处理者
//! 本来就是联邦自己的出口。
//!
//! ## 现在实现也在模块里（2026-11 S11）
//!
//! 本模块只是**薄桥**：把 `ports.admin`（宿主的状态迁移出口）交出去，
//! 由 [`crate::admin::dispatch`] 做真正的分发。规则（token 继承 / 冲突 /
//! URL 形状 / 邀请地址推导）全在 `admin.rs` 的纯函数里，状态迁移（写配置、
//! 断链、落 peer 表）由宿主的 `FederationAdminHost` 实现。
//!
//! 于是"哪条命令归谁、判什么、失败说什么"完整地在本 crate 内可读、可测；
//! 宿主那边只剩一个 `impl FederationAdminHost`。

use std::sync::Arc;

use crate::service::FederationEvent;

use crate::admin;
use crate::ports::FederationPorts;

/// 本模块处理的命令名（真源在 [`admin::COMMANDS`]）。
pub const COMMANDS: [&str; 6] = admin::COMMANDS;

/// 命令处理器：把命令交给规则层，状态迁移经 [`FederationAdminHost`]。
pub struct FederationCommandBridge {
    ports: Arc<FederationPorts>,
}

impl FederationCommandBridge {
    /// `emit` 是**进程级**事件出口（装配期捕获）：命令的副作用是发事件，
    /// 而"发给哪个 agent"在去主智能体之后不再是问题——所有 persona 共用
    /// 同一个汇聚点（见 `ports` 模块文档）。
    pub fn new(ports: Arc<FederationPorts>) -> Self {
        Self { ports }
    }

    /// 处理一条命令；`None` = 不是本域。
    pub async fn handle(
        &self,
        name: &str,
        payload: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        if !COMMANDS.contains(&name) {
            return None;
        }
        match self.ports.admin.clone() {
            Some(admin) => admin::dispatch_service(admin.as_ref(), name, payload).await,
            None => Some(Ok(serde_json::to_value(FederationEvent::Error {
                session_id: None,
                message: "联邦未启用（[federation] enabled = false）".into(),
            })
            .unwrap_or(serde_json::Value::Null))),
        }
    }
}
