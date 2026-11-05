//! 工作区模块的**宿主侧残留**（store 与命令域都已在模块仓里）。
//!
//! 模块（独立仓 `echo-module-workspace`）拥有会话文档、git 采集、文件浏览、
//! 命令域与提示词文案；宿主只保留与**会话子系统**绑定的三件事：
//!
//! 1. [`Agent::ensure_workspace_channel_for_active`]：把"激活的工作区"变成一条
//!    会话通道（`TrunkStore` 的操作）。通道是宿主的会话模型，模块不该认识它；
//! 2. [`Agent::workspace_after_change`]：收到模块快照后的落点——确保通道、
//!    恢复会话级 API 覆盖、广播 `SessionUpdated` / `WorkspaceSessions`；
//! 3. `emit_workspace_*`：两个广播辅助（被上面两处使用）。
//!
//! **数据从哪来**：模块每次成功变更 `emit workspace.changed`，组合根的事件桥
//! 把快照写进 [`crate::agent::workspace_mirror::WorkspaceMirror`] 再调
//! [`Agent::apply_workspace_snapshot`]。宿主只读镜像，永不写 store——单向数据流，
//! 不给"两份状态分叉"留缝。
//!
//! 历史：早先这里还有 565 行命令 match（Phase 3 ⑤ 迁进内置模块），2026-11 随
//! 模块独立成仓，命令域与 store 一起离开 Core；此处只剩会话通道这一半。

use crate::agent::Agent;
use crate::event::BackendEvent;

impl Agent {
    /// 确保当前激活工作区的本地通道会话已注册（激活 = 进入项目对话）。
    ///
    /// 返回刷新后的通道会话（昵称 = 最新工作区名），None = 未激活 / 未挂载
    /// 镜像。幂等：重复调用只刷昵称与活跃时间。
    pub(crate) fn ensure_workspace_channel_for_active(&self) -> Option<crate::session::Session> {
        let (active, conv) = self.workspace_mirror()?.snapshot().active_pair()?;
        // 多对话：通道 scope_id 由 (工作区, 对话) 派生——存量主对话 = 裸 ws id，
        // 其余对话 = `<ws>/<conv>`。派生规则与模块侧同源（模块仓 `store` 模块）。
        let scope = echo_module_workspace::store::channel_scope_id(&active.id, &conv);
        // 通道昵称：工作区名 + 对话名（多对话时在会话列表可区分）。
        let name = if conv.legacy {
            active.name.clone()
        } else {
            format!("{} · {}", active.name, conv.name)
        };
        Some(self.trunk.ensure_workspace_channel(&scope, &name))
    }

    /// 工作区状态变更后的统一广播（模块快照的落点）：
    ///
    /// 1. active 存在 → 确保通道会话注册并推送 `SessionUpdated`（面板据此
    ///    把通道加进会话列表/刷新昵称）；
    /// 2. 广播 `WorkspaceSessions`（列表 + 激活标记）——面板的「本地当前
    ///    对话」按 active 投影切换（见文档 §工作区会话与项目通道）。
    ///
    /// 面板命令与模型侧 `workspace` 工具（use/create/delete）最终都走到这里
    /// ——它们改的是模块里的 store，事件把变化送回宿主。
    pub(crate) async fn workspace_after_change(&self) {
        if let Some(session) = self.ensure_workspace_channel_for_active() {
            // 会话级 API 恢复（2026-10 持久化）：激活对话时若该对话有
            // `api_profile` 覆盖，重建会话级 provider；无覆盖时清除
            // （避免残留旧覆盖——例如从有覆盖的对话切到无覆盖的）。
            let session_id = session.id.clone();
            if let Some((_, conv)) = self
                .workspace_mirror()
                .and_then(|mirror| mirror.snapshot().active_pair())
            {
                if let Some(profile) = conv.api_profile.as_deref().filter(|p| !p.is_empty()) {
                    let _ = self.set_session_provider(&session_id, profile).await;
                } else {
                    let _ = self.set_session_provider(&session_id, "").await;
                }
            }
            self.emit(BackendEvent::SessionUpdated {
                session: session.info(String::new(), &self.node_identity()),
            });
        }
        self.emit_workspace_sessions();
    }

    /// Emit the current workspace session list snapshot（读镜像，不读 store）。
    fn emit_workspace_sessions(&self) {
        let Some(mirror) = self.workspace_mirror() else {
            self.emit_workspace_unavailable();
            return;
        };
        let snapshot = mirror.snapshot();
        let active_conversation = snapshot.active_pair().map(|(_, conv)| conv.id);
        self.emit(BackendEvent::WorkspaceSessions {
            team_id: self.team_id(),
            sessions: snapshot.sessions.iter().map(to_protocol_session).collect(),
            active: snapshot.active.clone(),
            active_conversation,
        });
    }

    /// 清空被删对话的通道历史（模块 `workspace.conversation_deleted` 的落点）。
    ///
    /// 删除对话 = 该对话的上下文不再可达，历史没有任何入口能读——留着只占
    /// 事件日志。与面板 `ClearHistory` 同一路径（`TrunkStore::clear_session`）。
    pub fn clear_workspace_conversation_channel(
        &self,
        workspace_id: &str,
        conversation: &echo_module_workspace::wire::WorkspaceConversationInfo,
    ) {
        let scope = echo_module_workspace::store::channel_scope_id(workspace_id, conversation);
        let session_id = crate::session::SessionKey::local_workspace(&scope).to_session_id();
        let trunk = self.trunk.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                trunk.clear_session(&session_id).await;
                tracing::info!(%session_id, "workspace conversation channel cleared on delete");
            });
        }
    }

    fn emit_workspace_error(&self, message: String) {
        self.emit(BackendEvent::Error {
            session_id: None,
            message,
        });
    }

    fn emit_workspace_unavailable(&self) {
        self.emit_workspace_error("workspace 模块未启用（该智能体未挂载工作区会话管理）".into());
    }
}

/// 模块线类型 → 宿主线类型（字段逐字一致；这里是**唯一的**转换点）。
///
/// 为什么要有转换而不是共用一套类型：模块仓被 Core 以 git 依赖引入，反向依赖
/// Core 的 `echo-protocol` 就是包级循环依赖（Cargo 拒绝）。两侧各自声明镜像，
/// 由这一处把边界收紧——将来任一侧加字段，编译器会在这里报错，而不是让面板
/// 静默丢字段。
fn to_protocol_session(
    session: &echo_module_workspace::wire::WorkspaceSessionInfo,
) -> echo_protocol::WorkspaceSessionInfo {
    echo_protocol::WorkspaceSessionInfo {
        id: session.id.clone(),
        name: session.name.clone(),
        description: session.description.clone(),
        directories: session
            .directories
            .iter()
            .map(|dir| match dir.node() {
                Some(node) => echo_protocol::WorkspaceDirectory::Qualified {
                    path: dir.path().to_string(),
                    node: Some(node.to_string()),
                },
                None => echo_protocol::WorkspaceDirectory::Local(dir.path().to_string()),
            })
            .collect(),
        conversations: session
            .conversations
            .iter()
            .map(|conv| echo_protocol::WorkspaceConversationInfo {
                id: conv.id.clone(),
                name: conv.name.clone(),
                legacy: conv.legacy,
                api_profile: conv.api_profile.clone(),
            })
            .collect(),
        last_conversation: session.last_conversation.clone(),
    }
}
