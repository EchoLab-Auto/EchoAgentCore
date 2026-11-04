//! 工作区状态**镜像**（宿主侧）。
//!
//! ## 为什么宿主不再持有 store
//!
//! 工作区插件已迁出为独立仓库（`echo-plugin-workspace`）：会话文档、git 采集、
//! 文件浏览、提示词文案、命令域全在那边，**store 也随之归插件所有**。
//!
//! 宿主这边剩下的只有两件与"会话子系统"绑定的事：
//!
//! 1. 激活工作区 = 进入项目对话通道 —— 通道（`TrunkStore` 的 workspace channel）
//!    是宿主的会话模型，插件不认识它；
//! 2. 列表/激活变化要广播（`WorkspaceSessions` / `SessionUpdated`）。
//!
//! 这两件事都只需要**读一份状态快照**（active / active_conversation / sessions），
//! 不需要写 store、不需要 git、不需要文件系统。所以宿主留一个只读镜像：
//! 插件每次成功变更 emit `workspace.changed`，[`WorkspaceMirror::replace`] 收下
//! 快照，宿主的既有广播路径照原样跑。
//!
//! 这是**单向**数据流的落点：插件是唯一写者，宿主只读。宿主从此不可能与插件
//! 的持久化状态分叉（此前两份 store 实例靠"不要同时写"的纪律维持一致）。

use std::sync::RwLock;

use echo_plugin_workspace::wire::{
    WorkspaceConversationInfo, WorkspaceSessionInfo,
};

/// `workspace.changed` 事件携带的状态快照。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct WorkspaceSnapshot {
    /// 激活的工作区 id（`None` = 未激活）。
    pub active: Option<String>,
    /// 激活对话 id（`active` 为 `None` 时忽略）。
    pub active_conversation: Option<String>,
    /// 全部会话（面板列表数据源）。
    pub sessions: Vec<WorkspaceSessionInfo>,
}

impl WorkspaceSnapshot {
    /// 从插件事件载荷解析（`{team_id, active, active_conversation, sessions}`）。
    ///
    /// 解析失败 = 插件与宿主的线格式漂移，记日志后按"空快照"处理——绝不 panic
    /// （插件事件在进程级出口上，一条坏载荷不能带崩宿主）。
    pub fn from_event_payload(payload: &serde_json::Value) -> Self {
        match serde_json::from_value::<Self>(payload.clone()) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "workspace.changed 载荷解析失败，按空快照处理（插件与宿主线格式不一致？）"
                );
                Self::default()
            }
        }
    }

    /// 激活的 `(工作区, 对话)`（读取期惰性补位：`active_conversation` 无效时
    /// 回退该工作区首条对话 / 存量主对话）。
    ///
    /// 补位规则与插件侧一致（`store::conversations_view`）——插件通常在事件里
    /// 已经给了解析后的 conversation id，这里的补位只防"事件丢失/竞态"。
    pub fn active_pair(&self) -> Option<(WorkspaceSessionInfo, WorkspaceConversationInfo)> {
        let active_id = self.active.as_deref()?;
        let session = self.sessions.iter().find(|s| s.id == active_id)?;
        let conversations = if session.conversations.is_empty() {
            vec![WorkspaceConversationInfo {
                id: "main".into(),
                name: "主对话".into(),
                legacy: true,
                api_profile: None,
            }]
        } else {
            session.conversations.clone()
        };
        let conversation = self
            .active_conversation
            .as_deref()
            .and_then(|id| conversations.iter().find(|c| c.id == id))
            .or_else(|| conversations.first())?;
        Some((session.clone(), conversation.clone()))
    }
}

/// 只读镜像槽（克隆共享；插件变更是唯一写者）。
#[derive(Debug, Default)]
pub struct WorkspaceMirror {
    snapshot: RwLock<WorkspaceSnapshot>,
}

impl WorkspaceMirror {
    /// 空镜像（未收到任何事件：表现为"未激活 + 无会话"）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前快照（克隆）。
    pub fn snapshot(&self) -> WorkspaceSnapshot {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 用插件推来的快照整体替换（事件即全量，无增量语义）。
    pub fn replace(&self, snapshot: WorkspaceSnapshot) {
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = snapshot;
    }

    /// 是否已收到过任何插件事件（诊断/测试用）。
    pub fn is_empty(&self) -> bool {
        let snapshot = self.snapshot.read().unwrap_or_else(|e| e.into_inner());
        snapshot.active.is_none() && snapshot.sessions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_plugin_workspace::wire::WorkspaceDirectory;

    /// 事件载荷 → 快照：字段名与插件侧一致（`active` / `active_conversation`
    /// / `sessions`），`active: null` 表示未激活。
    #[test]
    fn parses_changed_payload() {
        let payload = serde_json::json!({
            "team_id": "assistant",
            "active": "proj",
            "active_conversation": "main",
            "sessions": [{
                "id": "proj",
                "name": "Proj",
                "directories": ["/srv/a"]
            }]
        });
        let snapshot = WorkspaceSnapshot::from_event_payload(&payload);
        assert_eq!(snapshot.active.as_deref(), Some("proj"));
        let (session, conversation) = snapshot.active_pair().expect("active pair");
        assert_eq!(session.id, "proj");
        assert_eq!(conversation.id, "main");
        assert!(conversation.legacy, "空 conversations 惰性补位存量主对话");
    }

    /// 激活标记指向不存在的对话时补位到首条（事件竞态兜底）。
    #[test]
    fn active_pair_falls_back_to_first_conversation() {
        let snapshot = WorkspaceSnapshot {
            active: Some("proj".into()),
            active_conversation: Some("ghost".into()),
            sessions: vec![WorkspaceSessionInfo {
                id: "proj".into(),
                name: "Proj".into(),
                description: String::new(),
                directories: vec![WorkspaceDirectory::local("/srv/a")],
                conversations: vec![WorkspaceConversationInfo {
                    id: "docs".into(),
                    name: "Docs".into(),
                    legacy: true,
                    api_profile: None,
                }],
                last_conversation: None,
            }],
        };
        let (_, conversation) = snapshot.active_pair().expect("fallback");
        assert_eq!(conversation.id, "docs");
    }

    /// 未激活：`active_pair` 为 None，`is_empty` 为真。
    #[test]
    fn empty_mirror_reports_no_active() {
        let mirror = WorkspaceMirror::new();
        assert!(mirror.is_empty());
        assert!(mirror.snapshot().active_pair().is_none());
    }

    /// 镜像替换是全量语义：旧会话不残留。
    #[test]
    fn replace_is_wholesale() {
        let mirror = WorkspaceMirror::new();
        mirror.replace(WorkspaceSnapshot {
            active: None,
            active_conversation: None,
            sessions: vec![WorkspaceSessionInfo {
                id: "a".into(),
                name: "A".into(),
                ..Default::default()
            }],
        });
        assert_eq!(mirror.snapshot().sessions.len(), 1);
        mirror.replace(WorkspaceSnapshot::default());
        assert!(mirror.snapshot().sessions.is_empty());
    }
}
