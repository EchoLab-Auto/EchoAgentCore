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
        #[serde(default)]
        disabled_plugins: Vec<String>,
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
    },
    /// Delete a team member (the default/main agent is protected).
    DeleteTeam { id: String },
    /// Enable or disable a plugin at runtime. Disabled plugins are unmounted
    /// (registrations disposed); enabled ones are remounted. Persisted via
    /// `[agent].disabled_plugins`.
    TogglePlugin { id: String, enabled: bool },
    /// Request a snapshot of the current trunk context (`BackendEvent::ContextSnapshot`).
    RequestContext {
        #[serde(default)]
        team_id: Option<String>,
    },
    /// Cancel running work for a session (agent turns + background tasks).
    /// `all` = true cancels every active turn; false cancels only the newest.
    CancelRequestedWork { session_id: String, all: bool },
    /// Request the persisted display timeline (`BackendEvent::TrunkTimeline`).
    /// The TUI sends this on startup to restore historical messages.
    /// `agent_id` selects the persona timeline (None = management/default).
    RequestTrunkTimeline {
        #[serde(default)]
        team_id: Option<String>,
    },
    /// Erase all agent conversation memory: the durable session event log,
    /// the in-memory trunk context and the persisted display timeline.
    /// Responds with a fresh (empty) `TrunkTimeline` + `ContextSnapshot`.
    /// **Frontend-only** — destructive, human-in-the-loop action; the agent
    /// must never wipe its own memory.
    ClearHistory,
    /// Start an adapter by name.
    StartAdapter { name: String },
    /// Stop an adapter by name.
    StopAdapter { name: String },
    /// Restart an adapter.
    RestartAdapter { name: String },
    /// Request the current adapter status list.
    RequestAdapterStatus,
    /// Request the QQ group list.
    RequestGroupList,
    /// Request the QQ friend list (for interactive allowlist/denylist UI).
    RequestFriendList,
    /// Start all configured adapters.
    StartAllAdapters,
    /// Stop all running adapters.
    StopAllAdapters,
    /// Update QQ adapter allowlist at runtime.
    UpdateQqAllowlist {
        user_ids: Vec<i64>,
        group_ids: Vec<i64>,
    },
    /// Request the list of all discovered skills (metadata + instructions).
    /// Responds with `BackendEvent::SkillsList`.
    RequestSkillsList,
    /// Request the list of all registered tool definitions.
    /// Responds with `BackendEvent::ToolsList`.
    RequestToolsList,
    /// Update QQ adapter denylist at runtime.
    UpdateQqDenylist {
        user_ids: Vec<i64>,
        group_ids: Vec<i64>,
    },
    /// Request current QQ filter config.
    RequestQqFilterConfig,
    /// Set QQ gating mode. Serialised as "none" / "allowlist" / "denylist".
    SetQqGateMode { mode: GateMode },
    /// Set the QQ owner (admin) at runtime. Persisted through the Core's
    /// shared ConfigStore. **Frontend-only** — never exposed to the agent/LLM.
    SetQqOwner { owner_qq: i64 },
    /// Request the current QQ owner (admin) QQ number. **Frontend-only** —
    /// never exposed to the agent/LLM.
    RequestQqOwner,
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
        | BackendCommand::RequestQqOwner
        | BackendCommand::RequestQqFilterConfig
        | BackendCommand::ClearHistory => CommandClearance::Frontend,
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
                group_ids: vec![]
            }),
            CommandClearance::Frontend
        );
        assert_eq!(
            command_clearance(&BackendCommand::UpdateQqDenylist {
                user_ids: vec![],
                group_ids: vec![]
            }),
            CommandClearance::Frontend
        );
        assert_eq!(
            command_clearance(&BackendCommand::SetQqGateMode {
                mode: crate::mode::GateMode::Allowlist
            }),
            CommandClearance::Frontend
        );
        assert_eq!(
            command_clearance(&BackendCommand::SetQqOwner { owner_qq: 123 }),
            CommandClearance::Frontend
        );
        assert_eq!(
            command_clearance(&BackendCommand::RequestQqOwner),
            CommandClearance::Frontend
        );
        assert_eq!(
            command_clearance(&BackendCommand::RequestQqFilterConfig),
            CommandClearance::Frontend
        );
        assert_eq!(
            command_clearance(&BackendCommand::ClearHistory),
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
