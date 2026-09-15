//! Commands sent by the TUI / API layer into the agent backend.

use crate::mode::{GateMode, ReasoningEffort, ThinkingMode};

/// Commands from the frontend to the agent backend.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BackendCommand {
    /// Send a message into a session (as if from the local user).
    /// `images` carries image URLs / data URIs for multimodal models.
    /// `team_id` routes the message to a team member (None = default).
    SendMessage {
        session_id: String,
        content: String,
        #[serde(default)]
        images: Vec<String>,
        #[serde(default)]
        team_id: Option<String>,
    },
    /// Switch the active model.
    SwitchModel { model: String },
    /// Switch the active provider.
    SwitchProvider { provider: String },
    /// Replace the system prompt (owned by the Core plugin, not the API config).
    SetSystemPrompt { prompt: String },
    /// Request the current system prompt plugin text.
    RequestSystemPrompt,
    /// Update an API profile (or the top-level default when `name` is empty).
    /// The profile is activated after saving. An empty `api_key` keeps the
    /// current key unchanged.
    UpdateApiConfig {
        name: String,
        provider: String,
        model: String,
        base_url: String,
        api_key: String,
        #[serde(default)]
        thinking: Option<ThinkingMode>,
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
    },
    /// Switch to an API profile by name (empty = top-level default).
    SwitchApi { name: String },
    /// Test connectivity of an API config: `name` empty = top-level default,
    /// otherwise that profile. Replies with `BackendEvent::ApiTestResult`.
    TestApi { name: String },
    /// Query account balance of an API config (DeepSeek endpoint only):
    /// `name` empty = top-level default, otherwise that profile.
    /// Replies with `BackendEvent::ApiBalanceResult`.
    QueryApiBalance { name: String },
    /// Delete an API profile by name.
    DeleteApi { name: String },
    /// Request a full state snapshot (responded with `BackendEvent::SessionUpdated`
    /// per session plus a fresh [`crate::BackendState`] via the bridge).
    RequestState,
    /// Enable or disable a skill.
    ToggleSkill { name: String, enabled: bool },
    /// Enable or disable a registered tool at runtime. Disabled tools are
    /// hidden from the model and calls fail; persisted via `[agent]`.
    ToggleTool { name: String, enabled: bool },
    /// Create or overwrite a `SKILL.md` file in the configured skills
    /// directory. `name` is the directory/file key; content is the markdown
    /// body (the frontmatter is regenerated from the metadata fields).
    SaveSkill {
        name: String,
        description: String,
        keywords: Vec<String>,
        #[serde(default)]
        always: bool,
        #[serde(default)]
        category: String,
        content: String,
        /// 系统提示词 skill（system: true）：内容注入 base 区。
        #[serde(default)]
        system: bool,
    },
    /// Delete a skill directory under the configured skills directory.
    DeleteSkill { name: String },
    /// 归档某个 team 的历史会话（快照到 archives/ 并清空当前历史）。
    ArchiveHistory {
        #[serde(default)]
        team_id: Option<String>,
    },
    /// 压缩某个 team 的历史：旧事件替换为规则摘要（保留最近 keep_recent 条）。
    CompactHistory {
        #[serde(default)]
        team_id: Option<String>,
        #[serde(default)]
        keep_recent: Option<usize>,
    },
    /// Request the list of all mounted plugins.
    /// Responds with `BackendEvent::PluginsList`.
    RequestPluginsList,
    /// Request the list of team members (multi-agent).
    /// Responds with `BackendEvent::TeamsList`.
    RequestTeamsList,
    /// Enable or disable a team member at runtime (unloads memory/logs).
    ToggleTeam { id: String, enabled: bool },
    /// Create or update a team member (persisted to [agent].teams).
    SaveTeam {
        id: String,
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        system_prompt: String,
        #[serde(default = "default_true_agent")]
        enabled: bool,
        /// 该 agent 系统提示词由哪些 skill 组成（空 = 用 system_prompt 字段）。
        #[serde(default)]
        system_skills: Vec<String>,
        #[serde(default)]
        disabled_tools: Vec<String>,
        #[serde(default)]
        disabled_skills: Vec<String>,
        #[serde(default)]
        enabled_plugins: Vec<String>,
        #[serde(default)]
        enabled_tools: Vec<String>,
        #[serde(default)]
        enabled_skills: Vec<String>,
        /// Per-agent trunk token budget (None = 继承全局 [agent].memory_limit_tokens)。
        #[serde(default)]
        memory_limit_tokens: Option<usize>,
        /// Per-agent context window cap (None = 继承全局 [agent].context_window_tokens)。
        #[serde(default)]
        context_window_tokens: Option<usize>,
        /// Persona 级 API 供应商引用（None = 跟随全局默认配置；
        /// Some(name) = 使用全局供应商池 `[agent].api_profiles` 中的该 profile）。
        /// 保存在 `[agent.teams.{id}].api_profile`，运行期即时重建该 persona 的 provider。
        #[serde(default)]
        api_profile: Option<String>,
    },
    /// Delete a team member (the default/main agent is protected).
    DeleteTeam { id: String },
    /// Enable or disable a plugin at runtime. Persisted via
    /// `[agent].disabled_plugins`.
    /// 注：Phase 1 内置插件的 mount 尚无真实副作用，本命令当前对内置插件
    /// 只影响 Panel 展示与持久化状态；三个 UI 门控插件（branch.reply /
    /// session.global / chatbot.sessions）的禁用有实际效果。数据插件的
    /// 真实注册卸载随 Phase 2 落地。
    TogglePlugin { id: String, enabled: bool },
    /// Request a snapshot of a conversation context (`BackendEvent::ContextSnapshot`).
    ///
    /// 多会话（2026-09）：`session_id` 指定要查看的会话上下文；缺省回退
    /// 该智能体的本地 TUI 会话（再回退任一已有会话）。
    RequestContext {
        #[serde(default)]
        team_id: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },
    /// Cancel running work for a session (agent turns + background tasks).
    /// `all` = true cancels every active turn; false cancels only the newest.
    CancelRequestedWork {
        session_id: String,
        all: bool,
        /// 目标 agent（None/未指定 = 默认 agent）。
        /// 取消必须路由到 turn 实际所在的 agent：self-coding 的 turn 在
        /// self-coding 的 Agent 中，默认 agent 取消永远为 0。
        #[serde(default)]
        team_id: Option<String>,
    },
    /// Request the persisted display timeline (`BackendEvent::TrunkTimeline`).
    /// The TUI sends this on startup to restore historical messages.
    /// `agent_id` selects the persona timeline (None = management/default).
    /// 从外部 Git 仓库安装 skill（clone 到 skills_dir，热重载自动发现）。
    InstallSkillFromGit {
        /// git 仓库 URL（支持 https/ssh/本地路径）
        url: String,
        /// 安装目录名（缺省取仓库名）
        #[serde(default)]
        name: Option<String>,
        /// 分支/标签（缺省 = 仓库默认分支）
        #[serde(default)]
        branch: Option<String>,
    },
    /// 更新从 Git 安装的 skill（根据 sources 记录 fetch + reset）。
    UpdateSkillFromGit { name: String },
    /// 移除 Git 来源记录（不删除目录；目录删除仍用 DeleteSkill）。
    RemoveSkillSource { name: String },
    RequestTrunkTimeline {
        #[serde(default)]
        team_id: Option<String>,
        /// 增量拉取：仅返回 seq > since_seq 的 timeline 条目并携带最新 seq。
        /// None / 0 = 全量（默认）。前端首次加载全量，此后切换/回放只拉增量。
        #[serde(default)]
        since_seq: u64,
    },
    /// 请求所有后台 shell 会话列表。
    RequestShellSessions,
    /// 新建一个后台 shell 会话（持久 bash，可反复执行命令）。
    ShellStart {
        #[serde(default)]
        workdir: Option<String>,
    },
    /// 在指定 shell 会话中执行一条命令（超时内返回输出；超时标注且会话保留）。
    ShellExec {
        session_id: String,
        command: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    /// 停止（销毁）一个后台 shell 会话。
    ShellStop { session_id: String },
    /// Erase all agent conversation memory: the durable session event log,
    /// the in-memory trunk context and the persisted display timeline.
    /// Responds with a fresh (empty) `TrunkTimeline` + `ContextSnapshot`.
    /// **Frontend-only** — destructive, human-in-the-loop action; the agent
    /// must never wipe its own memory.
    ClearHistory {
        #[serde(default)]
        team_id: Option<String>,
    },
    /// Start an adapter by name.
    StartAdapter { name: String },
    /// Stop an adapter by name.
    StopAdapter { name: String },
    /// Restart an adapter.
    RestartAdapter { name: String },
    /// Request the current adapter status list.
    RequestAdapterStatus,
    /// Request the QQ group list.
    RequestGroupList {
        #[serde(default)]
        adapter: Option<String>,
    },
    /// Request the QQ friend list (for interactive allowlist/denylist UI).
    RequestFriendList {
        #[serde(default)]
        adapter: Option<String>,
    },
    /// 查询某 QQ 实例的登录状态（Core 代理，Panel 不直连 OneBot）。
    RequestQqLoginStatus {
        #[serde(default)]
        adapter: Option<String>,
    },
    /// 拉取某 QQ 实例的登录二维码（Core 代理：从 NapCat 容器取 PNG）。
    RequestQqQrcode {
        #[serde(default)]
        adapter: Option<String>,
    },

    // ---- 工作区会话（workspace 插件：基于工作空间的会话管理）----
    /// 请求某 persona 的工作区会话列表。
    /// 响应 `BackendEvent::WorkspaceSessions`。
    RequestWorkspaceSessions {
        #[serde(default)]
        team_id: Option<String>,
    },
    /// 新建/更新一个工作区会话（按 `session.id` upsert；空 id 由服务端
    /// 依名称生成）。响应为刷新后的 `WorkspaceSessions` 列表。
    SaveWorkspaceSession {
        #[serde(default)]
        team_id: Option<String>,
        session: crate::event::WorkspaceSessionInfo,
    },
    /// 删除一个工作区会话（若为激活会话则同时清除激活标记）。
    DeleteWorkspaceSession {
        #[serde(default)]
        team_id: Option<String>,
        id: String,
    },
    /// 激活（或 `id = None` 取消激活）一个工作区会话。
    /// 激活会话的工作目录会注入系统提示词。
    ActivateWorkspaceSession {
        #[serde(default)]
        team_id: Option<String>,
        #[serde(default)]
        id: Option<String>,
    },
    /// 请求某会话各工作区目录的 git 状态（`git` CLI 采集，只读）。
    /// 响应 `BackendEvent::WorkspaceGitStatus`。
    RequestWorkspaceGitStatus {
        #[serde(default)]
        team_id: Option<String>,
        session_id: String,
    },
    /// 请求列出某工作区目录下的文件与子目录（文件浏览器，只读）。
    /// `path` 必须是该会话某个工作区目录本身或其后代（服务端以
    /// canonical 路径前缀校验，越界拒绝）。响应 `BackendEvent::WorkspaceFiles`。
    RequestWorkspaceFiles {
        #[serde(default)]
        team_id: Option<String>,
        session_id: String,
        path: String,
    },
    /// Start all configured adapters.
    StartAllAdapters,
    /// Stop all running adapters.
    StopAllAdapters,
    /// Update QQ adapter allowlist at runtime.
    UpdateQqAllowlist {
        /// QQ 实例名（多实例寻址；None = 唯一实例/legacy "qq"）。
        #[serde(default)]
        adapter: Option<String>,
        user_ids: Vec<i64>,
        group_ids: Vec<i64>,
    },
    /// Request the list of all discovered skills (metadata + instructions).
    /// Responds with `BackendEvent::SkillsList`.
    RequestSkillsList,
    /// 手动重新发现技能目录中的 SKILL.md 并刷新注册表（替代定时扫描）。
    /// Responds with `BackendEvent::SkillsList`.
    ReloadSkills,
    /// Request the list of all registered tool definitions.
    /// Responds with `BackendEvent::ToolsList`.
    RequestToolsList,
    /// Update QQ adapter denylist at runtime.
    UpdateQqDenylist {
        /// QQ 实例名（多实例寻址；None = 唯一实例）。
        #[serde(default)]
        adapter: Option<String>,
        user_ids: Vec<i64>,
        group_ids: Vec<i64>,
    },
    /// Request current QQ filter config.
    RequestQqFilterConfig {
        #[serde(default)]
        adapter: Option<String>,
    },
    /// Set QQ gating mode. Serialised as "none" / "allowlist" / "denylist".
    SetQqGateMode {
        mode: GateMode,
        /// QQ 实例名（多实例寻址；None = 唯一实例）。
        #[serde(default)]
        adapter: Option<String>,
    },
    /// Set the QQ owner (admin) at runtime. Persisted through the Core's
    /// shared ConfigStore. **Frontend-only** — never exposed to the agent/LLM.
    SetQqOwner {
        owner_qq: i64,
        #[serde(default)]
        adapter: Option<String>,
    },
    /// Request the current QQ owner (admin) QQ number. **Frontend-only** —
    /// never exposed to the agent/LLM.
    RequestQqOwner {
        #[serde(default)]
        adapter: Option<String>,
    },
}

/// Who is allowed to issue a given command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandClearance {
    /// Commands any frontend (Panel/TUI/API) may send.
    Frontend,
    /// Commands the agent/LLM may also trigger through its own tools.
    ///
    /// Reserved for future agent-originated command paths; today the agent
    /// never constructs [`BackendCommand`] values directly.
    Agent,
}

/// Return the command clearance level for `cmd`.
///
/// QQ gate/admin mutations are **Frontend** only: the agent has no tool that
/// updates allowlists/denylists, changes the gate mode, or sets/queries the
/// owner QQ. These are human-in-the-loop configuration surfaces and must not
/// be controllable by the model.
pub fn command_clearance(cmd: &BackendCommand) -> CommandClearance {
    match cmd {
        BackendCommand::UpdateQqAllowlist { .. }
        | BackendCommand::UpdateQqDenylist { .. }
        | BackendCommand::SetQqGateMode { .. }
        | BackendCommand::SetQqOwner { .. }
        | BackendCommand::RequestQqOwner { .. }
        | BackendCommand::RequestQqFilterConfig { .. }
        | BackendCommand::SaveWorkspaceSession { .. }
        | BackendCommand::DeleteWorkspaceSession { .. }
        | BackendCommand::ActivateWorkspaceSession { .. }
        | BackendCommand::ClearHistory { .. } => CommandClearance::Frontend,
        _ => CommandClearance::Agent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qq_admin_commands_are_frontend_only() {
        assert_eq!(
            command_clearance(&BackendCommand::UpdateQqAllowlist {
                user_ids: vec![],
                group_ids: vec![],
                adapter: None
            }),
            CommandClearance::Frontend
        );
        assert_eq!(
            command_clearance(&BackendCommand::UpdateQqDenylist {
                user_ids: vec![],
                group_ids: vec![],
                adapter: None
            }),
            CommandClearance::Frontend
        );
        assert_eq!(
            command_clearance(&BackendCommand::SetQqGateMode {
                mode: crate::mode::GateMode::Allowlist,
                adapter: None
            }),
            CommandClearance::Frontend
        );
        assert_eq!(
            command_clearance(&BackendCommand::SetQqOwner {
                owner_qq: 123,
                adapter: None
            }),
            CommandClearance::Frontend
        );
        assert_eq!(
            command_clearance(&BackendCommand::RequestQqOwner { adapter: None }),
            CommandClearance::Frontend
        );
        assert_eq!(
            command_clearance(&BackendCommand::RequestQqFilterConfig { adapter: None }),
            CommandClearance::Frontend
        );
        assert_eq!(
            command_clearance(&BackendCommand::ClearHistory { team_id: None }),
            CommandClearance::Frontend,
            "wiping memory is a human decision, never agent-originated"
        );
        assert_eq!(
            command_clearance(&BackendCommand::RequestAdapterStatus),
            CommandClearance::Agent
        );
        assert_eq!(
            command_clearance(&BackendCommand::SendMessage {
                session_id: "s".into(),
                content: "hi".into(),
                images: vec![],
                team_id: None
            }),
            CommandClearance::Agent
        );
    }
}

fn default_true_agent() -> bool {
    true
}
