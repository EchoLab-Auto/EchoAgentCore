//! 工作区状态**镜像**（宿主侧）。
//!
//! ## 为什么宿主不再持有 store
//!
//! 工作区模块已迁出为独立仓库（`agent-module-workspace`）：会话文档、git 采集、
//! 文件浏览、提示词文案、命令域全在那边，**store 也随之归模块所有**。
//!
//! 宿主这边剩下的只有两件与"会话子系统"绑定的事：
//!
//! 1. 激活工作区 = 进入项目对话通道 —— 通道（`TrunkStore` 的 workspace channel）
//!    是宿主的会话模型，模块不认识它；
//! 2. 列表/激活变化要广播（`WorkspaceSessions` / `SessionUpdated`）。
//!
//! 这两件事都只需要**读一份状态快照**（active / active_conversation / sessions），
//! 不需要写 store、不需要 git、不需要文件系统。所以宿主留一个只读镜像：
//! 模块每次成功变更 emit `workspace.changed`，[`WorkspaceMirror::replace`] 收下
//! 快照，宿主的既有广播路径照原样跑。
//!
//! 这是**单向**数据流的落点：模块是唯一写者，宿主只读。宿主从此不可能与模块
//! 的持久化状态分叉（此前两份 store 实例靠"不要同时写"的纪律维持一致）。
//!
//! ## 冷启动的空窗（C3c）
//!
//! 镜像**只**消费 `workspace.changed`，而那是"变更时才有"的事件。进程重启后
//! 磁盘上明明存着激活会话，但没有任何变更发生 → 镜像停在空态：项目对话通道
//! 不注册、`SessionUpdated` 不广播，直到用户手动改一次工作区。恢复办法见
//! [`WorkspaceMirror::seed_if_unseeded`]（复用具成的读命令
//! `RequestWorkspaceSessions` 作拉取来源）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

use agent_module_workspace::wire::{WorkspaceConversationInfo, WorkspaceSessionInfo};

/// `workspace.changed` 事件携带的状态快照（也是 `WorkspaceSessions` 回执的
/// 同名三字段）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct WorkspaceSnapshot {
    /// 激活的工作区 id（`None` = 未激活）。
    pub active: Option<String>,
    /// 激活对话 id（`active` 为 `None` 时忽略）。
    pub active_conversation: Option<String>,
    /// 全部会话（面板列表数据源）。
    pub sessions: Vec<WorkspaceSessionInfo>,
}

impl WorkspaceSnapshot {
    /// 解析模块的全量快照。`sessions` 必须显式提供；合法空列表与坏载荷不可混淆。
    pub fn try_from_event_payload(payload: &serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value::<Self>(payload.clone())
    }

    /// 激活的 `(工作区, 对话)`（读取期惰性补位：`active_conversation` 无效时
    /// 回退该工作区首条对话 / 存量主对话）。
    ///
    /// 补位规则与模块侧一致（`store::conversations_view`）——模块通常在事件里
    /// 已经给了解析后的 conversation id，这里的补位只防"事件丢失/竞态"。
    pub fn active_pair(&self) -> Option<(WorkspaceSessionInfo, WorkspaceConversationInfo)> {
        let active_id = self.active.as_deref()?;
        let session = self.sessions.iter().find(|s| s.id == active_id)?;
        // 补位规则**归模块**：`conversations_view` 是那条规则的唯一实现
        // （空列表 → 存量主对话 `main` / `legacy: true`）。此前这里抄了一份，
        // 两份一致全靠注释提醒——模块改补位逻辑时宿主不会跟上，也不会报错。
        let conversations = agent_module_workspace::store::conversations_view(session);
        let conversation = self
            .active_conversation
            .as_deref()
            .and_then(|id| conversations.iter().find(|c| c.id == id))
            .or_else(|| conversations.first())?;
        Some((session.clone(), conversation.clone()))
    }
}

/// 只读镜像槽（克隆共享；模块变更是唯一写者）。
///
/// `seeded` = "镜像已经有过来源"（一次变更事件，或一次冷启动补种）。它只用来
/// 判 [`Self::seed_if_unseeded`] 该不该收下补种，不参与读路径。
#[derive(Debug, Default)]
pub struct WorkspaceMirror {
    snapshot: RwLock<WorkspaceSnapshot>,
    seeded: AtomicBool,
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

    /// 用模块推来的快照整体替换（事件即全量，无增量语义）。
    pub fn replace(&self, snapshot: WorkspaceSnapshot) {
        let mut slot = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
        *slot = snapshot;
        // 变更事件是权威写入：此后不再接受"补种"（见 `seed_if_unseeded`）。
        self.seeded.store(true, Ordering::Release);
    }

    /// **冷启动补种**：只在镜像还没有任何来源时收下快照，返回是否写入。
    ///
    /// 用途：进程重启后镜像是空的（没有 `workspace.changed` 可消费），宿主
    /// 用模块的 `RequestWorkspaceSessions` 回执补一次。
    ///
    /// 为什么是"只补一次"而不是"来了就盖"：现有事件与回执**都没有版本/序号**
    /// （`WorkspaceSessions` 与 `workspace.changed` 都只带快照本身），一条迟到的
    /// 回执与"更新的变更事件"在宿主侧无法区分先后。因此这里只承诺**可验证的
    /// 子集**：镜像尚未有来源时接受补种；一旦有来源（变更事件或首次补种），
    /// 后续回执一律不覆盖 —— 宁可留下旧快照等下一次变更事件，也不让一条乱序
    /// 回执把合法状态改回去。
    pub fn seed_if_unseeded(&self, snapshot: WorkspaceSnapshot) -> bool {
        let mut slot = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
        if self.seeded.load(Ordering::Acquire) {
            return false;
        }
        *slot = snapshot;
        self.seeded.store(true, Ordering::Release);
        true
    }

    /// 是否已收到过任何模块事件/补种（诊断/测试用）。
    pub fn is_seeded(&self) -> bool {
        self.seeded.load(Ordering::Acquire)
    }

    /// 是否已收到过任何模块事件（诊断/测试用）。
    pub fn is_empty(&self) -> bool {
        let snapshot = self.snapshot.read().unwrap_or_else(|e| e.into_inner());
        snapshot.active.is_none() && snapshot.sessions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_module_workspace::wire::WorkspaceDirectory;

    /// 事件载荷 → 快照：字段名与模块侧一致（`active` / `active_conversation`
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
        let snapshot = WorkspaceSnapshot::try_from_event_payload(&payload).unwrap();
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
    /// 镜像替换是全量语义：旧会话不残留，并且算"有来源"。
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
        assert!(mirror.is_seeded(), "变更事件也是来源");
        mirror.replace(WorkspaceSnapshot::default());
        assert!(mirror.snapshot().sessions.is_empty());
    }

    #[test]
    fn strict_parse_rejects_malformed_payload() {
        for payload in [
            serde_json::json!({ "sessions": "not-a-list" }),
            serde_json::json!({ "team_id": "t" }),
        ] {
            assert!(WorkspaceSnapshot::try_from_event_payload(&payload).is_err());
        }
        let empty = WorkspaceSnapshot::try_from_event_payload(&serde_json::json!({
            "sessions": [],
        }))
        .unwrap();
        assert!(empty.sessions.is_empty() && empty.active.is_none());
    }

    /// 冷启动补种：只有第一次生效；此后回执不覆盖已有状态。
    #[test]
    fn seed_applies_once_then_stops() {
        let mirror = WorkspaceMirror::new();
        assert!(!mirror.is_seeded());
        let first = WorkspaceSnapshot {
            active: Some("proj".into()),
            active_conversation: Some("main".into()),
            sessions: vec![WorkspaceSessionInfo {
                id: "proj".into(),
                name: "Proj".into(),
                ..Default::default()
            }],
        };
        assert!(mirror.seed_if_unseeded(first.clone()), "空镜像接受补种");
        assert_eq!(mirror.snapshot().active.as_deref(), Some("proj"));

        // 迟到的/重复的回执：不覆盖。
        let late = WorkspaceSnapshot {
            active: Some("ghost".into()),
            ..Default::default()
        };
        assert!(!mirror.seed_if_unseeded(late));
        assert_eq!(mirror.snapshot().active.as_deref(), Some("proj"));
    }

    /// 变更事件先到 → 补种一律不生效（乱序回执不覆盖较新的变更）。
    #[test]
    fn change_event_wins_over_late_seed() {
        let mirror = WorkspaceMirror::new();
        mirror.replace(WorkspaceSnapshot {
            active: Some("docs".into()),
            active_conversation: None,
            sessions: vec![WorkspaceSessionInfo {
                id: "docs".into(),
                name: "Docs".into(),
                ..Default::default()
            }],
        });
        assert!(
            !mirror.seed_if_unseeded(WorkspaceSnapshot {
                active: Some("proj".into()),
                ..Default::default()
            }),
            "已有来源时补种被拒绝"
        );
        assert_eq!(mirror.snapshot().active.as_deref(), Some("docs"));
    }
}
