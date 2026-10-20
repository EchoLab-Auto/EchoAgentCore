//! 工作区插件的**宿主侧残留**（解耦计划：命令域已迁入插件）。
//!
//! 命令处理（7 条 `*Workspace*`）与提示词块都已搬进
//! `echo-plugin-workspace`（`Contribution::Command` / `Contribution::Text`），
//! 这里只剩**宿主独有**的几件小事：
//!
//! 1. [`Agent::ensure_workspace_channel_for_active`]：把"激活的工作区"变成一条
//!    会话通道（`TrunkStore` 的操作）。通道是宿主的会话模型，插件不该认识它；
//! 2. [`Agent::workspace_after_change`]：store 变更钩子的落点——确保通道、
//!    恢复会话级 API 覆盖、广播 `SessionUpdated` / `WorkspaceSessions`；
//! 3. `emit_workspace_*`：三个广播辅助（被上面两处使用）。
//!
//! **为什么这些不搬**：它们动的是"会话通道"与"会话级 provider"，那是宿主的
//! 会话子系统。插件改 store → 钩子触发 → 宿主自己走完这套流程，所以插件侧
//! 完全不需要知道它们存在（见 `echo_plugin_workspace::commands` 的模块文档）。
//!
//! 历史（Phase 3 ⑤ → 2026-11 命令域迁移）：早先这里还有 565 行命令 match，
//! 注释写着"留在这里因为它们是 `impl Agent` 的方法"。命令域搬进插件后那个
//! 理由不再成立——插件通过 [`crate::agent::workspace_host::AgentWorkspaceHost`]
//! 拿到它真正需要的四项能力，剩下的确实是宿主自己的事。

use crate::agent::Agent;
use crate::event::BackendEvent;

impl Agent {
    /// 确保当前激活工作区的本地通道会话已注册（激活 = 进入项目对话）。
    ///
    /// 返回刷新后的通道会话（昵称 = 最新工作区名），None = 未激活 / 未挂载
    /// 存储。幂等：重复调用只刷昵称与活跃时间。
    pub(crate) fn ensure_workspace_channel_for_active(&self) -> Option<crate::session::Session> {
        let store = self.workspace_store()?;
        let (active, conv) = store.active_pair()?;
        // 多对话（2026-10）：通道 scope_id 由 (工作区, 对话) 派生——存量
        // 主对话 = 裸 ws id，其余对话 = `<ws>/<conv>`。
        let scope = crate::workspace::channel_scope_id(&active.id, &conv);
        // 通道昵称：工作区名 + 对话名（多对话时在会话列表可区分）。
        let name = if conv.legacy {
            active.name.clone()
        } else {
            format!("{} · {}", active.name, conv.name)
        };
        Some(self.trunk.ensure_workspace_channel(&scope, &name))
    }

    /// 工作区状态变更后的统一广播（store 变更钩子的唯一落点）：
    ///
    /// 1. active 存在 → 确保通道会话注册并推送 `SessionUpdated`（面板据此
    ///    把通道加进会话列表/刷新昵称）；
    /// 2. 广播 `WorkspaceSessions`（列表 + 激活标记）——面板的「本地当前
    ///    对话」按 active 投影切换（见文档 §工作区会话与项目通道）。
    ///
    /// 面板命令与模型侧 `workspace` 工具（use/create/delete）共用本路径。
    pub(crate) async fn workspace_after_change(&self) {
        if let Some(session) = self.ensure_workspace_channel_for_active() {
            // 会话级 API 恢复（2026-10 持久化）：激活对话时若该对话有
            // `api_profile` 覆盖，重建会话级 provider；无覆盖时清除
            // （避免残留旧覆盖——例如从有覆盖的对话切到无覆盖的）。
            let session_id = session.id.clone();
            if let Some(store) = self.workspace_store() {
                if let Some((_, conv)) = store.active_pair() {
                    if let Some(profile) = conv.api_profile.as_deref().filter(|p| !p.is_empty()) {
                        let _ = self.set_session_provider(&session_id, profile).await;
                    } else {
                        let _ = self.set_session_provider(&session_id, "").await;
                    }
                }
            }
            self.emit(BackendEvent::SessionUpdated {
                session: session.info(String::new(), &self.node_identity()),
            });
        }
        self.emit_workspace_sessions();
    }

    /// Emit the current workspace session list snapshot.
    fn emit_workspace_sessions(&self) {
        let Some(store) = self.workspace_store() else {
            self.emit_workspace_unavailable();
            return;
        };
        let (sessions, active) = store.snapshot();
        let active_conversation = store.active_pair().map(|(_, conv)| conv.id);
        self.emit(BackendEvent::WorkspaceSessions {
            team_id: self.team_id(),
            sessions,
            active,
            active_conversation,
        });
    }

    fn emit_workspace_error(&self, message: String) {
        self.emit(BackendEvent::Error {
            session_id: None,
            message,
        });
    }

    fn emit_workspace_unavailable(&self) {
        self.emit_workspace_error("workspace 插件未启用（该智能体未挂载工作区会话管理）".into());
    }
}
