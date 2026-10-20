//! Commands sent by the TUI / API layer into the agent backend.

use crate::mode::{ReasoningEffort, ThinkingMode};

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
    /// 会话级 API 覆盖（2026-10）：该会话的 LLM 请求走指定 profile
    /// 的 provider；`profile` 空串 = 清除覆盖（回到跟随 persona 级）。
    /// team_id 必填（去主智能体后会话类命令的显式归属）。
    SetSessionApi {
        team_id: Option<String>,
        session_id: String,
        profile: String,
    },
    /// 清除会话级 API 覆盖（回到跟随 persona 级）。
    ClearSessionApi {
        team_id: Option<String>,
        session_id: String,
    },
    /// 查询联邦远端节点的**脱敏**供应商池（分布式供应商共享，2026-10）：
    /// 经 `Query(ApiProfiles)` 拉取，回 `RemoteApiProfiles` 事件
    /// （provider/model/base_url/key_set——api_key 明文永不过线）。
    /// Frontend-only 管理命令。
    RequestRemoteApiProfiles { peer: String },
    /// Test connectivity of an API config: `name` empty = top-level default,
    /// otherwise that profile. Replies with `BackendEvent::ApiTestResult`.
    TestApi { name: String },
    /// Query account balance of an API config (DeepSeek endpoint only):
    /// `name` empty = top-level default, otherwise that profile.
    /// Replies with `BackendEvent::ApiBalanceResult`.
    QueryApiBalance { name: String },
    /// 请求 API profile 的**本地积累指标**（余额快照 + token 用量时间序列）：
    /// `name` 空 = 默认配置 + 全部 profile；非空 = 指定 profile。
    /// 回 `BackendEvent::ApiMetrics`（单事件，每个请求目标一条 entry）。
    QueryApiMetrics { name: String },
    /// Delete an API profile by name.
    DeleteApi { name: String },
    /// 原子改名 API profile：重写全部按名引用（供应商池条目名 / `active_api` /
    /// `teams[*].api_profile`），并重绑运行中的 persona 引用、会话覆盖与
    /// 工作区对话引用。`to` 去空白后必须非空且不与现有名字冲突；`from`
    /// 必须精确存在。成功回 `ApiConfigUpdated` + `ApiProfilesUpdated`
    /// （teams 有变更时补 `TeamsList`）；失败回 `Error` 且零写入。
    RenameApi { from: String, to: String },
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
    ///
    /// `session_id` 存在时**会话级**：只归档并清空该会话（2026-10 审计
    /// 修复——此前无会话维度，点一次误清同人格全部会话）；缺失时保持
    /// 旧语义（整档归档 + 全量清空），向后兼容旧面板。
    ArchiveHistory {
        #[serde(default)]
        team_id: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
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
    /// 重载一个插件（卸下 → 重新挂载），**不改**启用状态与持久化配置。
    ///
    /// 用途：插件按新配置/新外部依赖重新初始化（子进程插件重连、实例
    /// 插件重启进程）；管理面插件被拒绝（重启 = Panel 断连）。
    RestartPlugin { id: String },
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
        /// **首屏窗口上限**：全量快照最多返回**最新** N 条（`None` = 不限，
        /// 与旧行为一致）。
        ///
        /// 为什么要它：时间线上限 1024 条，压缩后约 0.6 MB——首屏/重连一次性
        /// 收完既拖慢首帧，也让"用户其实只想看最后一屏"的场景白付传输与解析。
        /// 前端传 200 级别的值即可，更早的内容按 [`Self::RequestTimelineEarlier`]
        /// 向上翻页（响应里的 `has_more` 表示还有没有）。
        ///
        /// 只影响 `since_seq == 0`（全量）路径：增量路径本来就是缺口补齐，
        /// 截断会让前端游标与实际内容不一致。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    /// 请求**更早的时间线**（向上翻页；2026-10 分页）。
    ///
    /// 与 [`Self::RequestTrunkTimeline`] 分开是因为语义不同：翻页只把更早的
    /// 条目**追加到前端列表头部**，不动同步游标（`seq`）；而 `TrunkTimeline`
    /// 的 `full` 语义是"替换缓存"。混在一个命令里会让前端难以判断该替换还是
    /// 该前插。
    RequestTimelineEarlier {
        #[serde(default)]
        team_id: Option<String>,
        /// **已加载的最老条目的位置序号**（`TimelineMessage.ord`）。
        ///
        /// 为什么不用 `seq` 作锚点：时间线的 `seq` **不是单调递增**的——Core
        /// 对运行中的工具条目做就地更新时会推进该条目的 `seq`（位置不变、
        /// 序号变大，增量同步才能重新投递它）。实测 1024 条里��� 46 处非单调，
        /// 按 `seq` 定位会过早截断并**漏条目**（逐页拼接只还原 842/1024）。
        ///
        /// 为什么也不用"已加载条数"：分页期间的新增会让"从尾部数 N 条"整体
        /// 错位，同样漏条目。
        ///
        /// `ord` 是 push 时分配、**此后永不改变**的位置序号，严格递增 → 既是
        /// 稳定锚点，也能二分定位。与内容、新增、就地更新全都无关。
        #[serde(default)]
        before_ord: u64,
        /// 本页最多返回条数（服务端 clamp [1, 500]）。
        #[serde(default)]
        limit: Option<usize>,
    },
    /// 请求某会话的**显示历史**（从事件日志按会话派生；2026-10 修复面板
    /// 「会话无消息」——共享显示时间线按 FIFO 淘汰后旧会话条目不可见）。
    /// `limit`：最多返回条目数（缺省 500；服务端 clamp [1, 2000]，取最近 N 条）。
    RequestSessionHistory {
        #[serde(default)]
        team_id: Option<String>,
        session_id: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// 请求后台 shell 会话列表（带 team_id 时只返回该 persona 的会话）。
    RequestShellSessions {
        #[serde(default)]
        team_id: Option<String>,
    },
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
    /// Erase agent conversation memory. Scope:
    /// - `session_id = Some(id)`：仅清理该会话（事件日志中归属该会话的
    ///   事件 + 该会话上下文投影 + 时间线中该会话条目）；
    /// - `session_id = None`：清空该人格全部会话（显式全清的管理语义）。
    ///
    /// Responds with a fresh `TrunkTimeline` + `ContextSnapshot`.
    ///
    /// **Frontend-only** — destructive, human-in-the-loop action; the agent
    /// must never wipe its own memory.
    ///
    /// 2026-10 用户报告修复：`session_id` 为新增维度——此前只有 team 维度，
    /// 面板「清理历史记忆」把同人格下**所有会话**（QQ 群/私聊、其他
    /// workspace 会话）的记忆一并清掉。
    ClearHistory {
        #[serde(default)]
        team_id: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },
    /// Start an instance by name.
    StartInstance { name: String },
    /// Stop an instance by name.
    StopInstance { name: String },
    /// Restart an instance.
    RestartInstance { name: String },
    /// Request the current instance status list.
    RequestInstanceStatus,
    /// 调用一个插件实例管理方法（W2.1，2026-10 插件实例管理通道）。
    ///
    /// **唯一新增的插件实例管理命令**——取代逐平台专属命令（旧 QQ 专属变体
    /// 已于 2026-10 直接删除）。
    /// `method` 与插件协议 `custom op` 及 `InstanceManagement::invoke_custom`
    /// 同名直通（如 `set_gate_mode` / `update_allowlist` / `login_qrcode`）。
    /// 响应为 [`crate::event::BackendEvent::InstanceOpResult`]。
    ///
    /// **Frontend-only**（类别判定，fail-closed）：一切插件实例管理命令都是
    /// 人在环配置面，模型绝不可控。
    InvokeInstanceOp {
        /// 插件实例名（旧线协议字段名 `adapter` 仍接受——2026-10 去插件实例化）。
        #[serde(alias = "adapter")]
        instance: String,
        /// 管理方法名（平台自定义；推荐名见 `InstanceManagement` 文档）。
        method: String,
        /// 方法参数（JSON；缺省空对象）。
        #[serde(default)]
        payload: serde_json::Value,
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
        /// 工作区内的对话 id（2026-10 多对话；None = 进入该工作区的
        /// `last_conversation` / 首条对话）。`id = None`（取消激活）时忽略。
        #[serde(default)]
        conversation: Option<String>,
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
    /// 目录选择器浏览（2026-10）：列出「浏览根」内的子目录，供新建/
    /// 编辑工作区时**选择**目录（与文件浏览器不同：不依赖会话已声明的
    /// 工作区目录——解决"要浏览才能选、要选才能浏览"的鸡生蛋问题）。
    ///
    /// `path` 形态：
    /// - `None` → 返回浏览根列表（roots：工作区目录并集 ∪ HOME）；
    /// - `Some("/abs")` → 列出该本机目录的子条目（限浏览根内）；
    /// - `Some("node://<peer>")` → 拉取对端浏览根（经联邦）；
    /// - `Some("node://<peer>/<abs>")` → 列出对端该目录的子条目。
    ///
    /// 浏览根内校验在服务端（canonical 前缀）。响应
    /// `BackendEvent::BrowseDirectories`。
    RequestBrowseDirectories {
        #[serde(default)]
        path: Option<String>,
    },
    /// Start all configured instances.
    StartAllInstances,
    /// Stop all running instances.
    StopAllInstances,
    /// Request the list of all discovered skills (metadata + instructions).
    /// Responds with `BackendEvent::SkillsList`.
    RequestSkillsList,
    /// 手动重新发现技能目录中的 SKILL.md 并刷新注册表（替代定时扫描）。
    /// Responds with `BackendEvent::SkillsList`.
    ReloadSkills,
    /// Request the list of all registered tool definitions.
    /// Responds with `BackendEvent::ToolsList`.
    RequestToolsList,

    // ── 联邦管理（federation Phase 4，**Frontend-only**）──
    /// 新增/更新一个联邦 peer（运行时生效 + ConfigStore 原子写回）。
    /// 响应为刷新后的 `FederationStatus`。
    SaveFederationPeer {
        peer: crate::event::FederationPeerInfo,
    },
    /// 删除一个联邦 peer（断链 + 配置移除）。
    DeleteFederationPeer { name: String },
    /// 请求联邦状态（开关/监听/node_id/peers + 各 peer 链路状态）。
    RequestFederationStatus,
    /// 生成本机邀请串（`echofed://`；对端粘贴后自动填 url/token）。
    /// 邀请串含一次性之外的静态 token——按「互信 ≈ SSH 免密」对待其分发。
    RequestFederationInvite,

    // ── 会话迁移（federation P3-2，**Frontend-only**）──
    /// 把会话从源节点迁移到目标节点：源节点导出 timeline（经联邦
    /// SessionSnapshot 查询）→ 目标节点导入为新会话 → 源节点归档
    /// （标记 migrated，不再活跃）。负载再平衡/节点维护用。
    MigrateSession {
        /// 会话 id（源节点本地 id，不含 node:// 前缀）。
        session_id: String,
        /// 会话归属人格（多 agent；None = 无活动人格时逐 persona 查找）。
        #[serde(default)]
        team_id: Option<String>,
        /// 目标 peer 配置名（`[federation.peers.<name>]`）。
        target_peer: String,
    },

    // ── 自更新（2026-10，**Frontend-only**）──
    /// 触发框架自更新：启动 `echo-agent-core-update.service`（oneshot，
    /// 脱离本进程执行 构建→替换→重启）。Graceful Drain 保证当前回复
    /// 完整生成后再重启；面板短暂重连后会话自动恢复。响应为
    /// `SelfUpdateStatus` 事件（状态轮询由前端继续）。
    RequestSelfUpdate,
    /// 查询自更新状态（读 `update-status` 文件 → `SelfUpdateStatus`）。
    RequestSelfUpdateStatus,
}

impl BackendCommand {
    /// 变体名的**零分配**取值（等于 serde 的线上 tag）。
    ///
    /// `BackendCommand` 没有 `rename_all`，所以线上 tag 就是 Rust 变体名——
    /// 本函数返回的字符串与 `serde_json::to_value(&cmd)` 的第一个键逐字相同
    /// （有测试守着这条等价关系）。
    ///
    /// 用途：命令分发的**插件前置查表**。插件按名字声明"我处理哪些命令"
    /// （`Contribution::Command`），宿主需要"不序列化就看得到名字"——
    /// `apply_command` 拿到的是已反序列化的枚举，为拿名字再 `to_value` 一遍
    /// 是纯浪费（而命令里有大载荷，比如整段消息）。
    ///
    /// **穷举 match，没有 `_` 分支**：新增命令变体时编译器会逼你在这里起名。
    /// 忘了起名会让插件永远收不到该命令，而这种静默失败比编译错误难查得多。
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::SendMessage { .. } => "SendMessage",
            Self::SwitchModel { .. } => "SwitchModel",
            Self::SwitchProvider { .. } => "SwitchProvider",
            Self::SetSystemPrompt { .. } => "SetSystemPrompt",
            Self::RequestSystemPrompt { .. } => "RequestSystemPrompt",
            Self::UpdateApiConfig { .. } => "UpdateApiConfig",
            Self::SwitchApi { .. } => "SwitchApi",
            Self::SetSessionApi { .. } => "SetSessionApi",
            Self::ClearSessionApi { .. } => "ClearSessionApi",
            Self::RequestRemoteApiProfiles { .. } => "RequestRemoteApiProfiles",
            Self::TestApi { .. } => "TestApi",
            Self::QueryApiBalance { .. } => "QueryApiBalance",
            Self::QueryApiMetrics { .. } => "QueryApiMetrics",
            Self::DeleteApi { .. } => "DeleteApi",
            Self::RenameApi { .. } => "RenameApi",
            Self::RequestState { .. } => "RequestState",
            Self::ToggleSkill { .. } => "ToggleSkill",
            Self::ToggleTool { .. } => "ToggleTool",
            Self::SaveSkill { .. } => "SaveSkill",
            Self::DeleteSkill { .. } => "DeleteSkill",
            Self::ArchiveHistory { .. } => "ArchiveHistory",
            Self::CompactHistory { .. } => "CompactHistory",
            Self::RequestPluginsList { .. } => "RequestPluginsList",
            Self::RequestTeamsList { .. } => "RequestTeamsList",
            Self::ToggleTeam { .. } => "ToggleTeam",
            Self::SaveTeam { .. } => "SaveTeam",
            Self::DeleteTeam { .. } => "DeleteTeam",
            Self::TogglePlugin { .. } => "TogglePlugin",
            Self::RestartPlugin { .. } => "RestartPlugin",
            Self::RequestContext { .. } => "RequestContext",
            Self::CancelRequestedWork { .. } => "CancelRequestedWork",
            Self::InstallSkillFromGit { .. } => "InstallSkillFromGit",
            Self::UpdateSkillFromGit { .. } => "UpdateSkillFromGit",
            Self::RemoveSkillSource { .. } => "RemoveSkillSource",
            Self::RequestTrunkTimeline { .. } => "RequestTrunkTimeline",
            Self::RequestTimelineEarlier { .. } => "RequestTimelineEarlier",
            Self::RequestSessionHistory { .. } => "RequestSessionHistory",
            Self::RequestShellSessions { .. } => "RequestShellSessions",
            Self::ShellStart { .. } => "ShellStart",
            Self::ShellExec { .. } => "ShellExec",
            Self::ShellStop { .. } => "ShellStop",
            Self::ClearHistory { .. } => "ClearHistory",
            Self::StartInstance { .. } => "StartInstance",
            Self::StopInstance { .. } => "StopInstance",
            Self::RestartInstance { .. } => "RestartInstance",
            Self::RequestInstanceStatus { .. } => "RequestInstanceStatus",
            Self::InvokeInstanceOp { .. } => "InvokeInstanceOp",
            Self::RequestWorkspaceSessions { .. } => "RequestWorkspaceSessions",
            Self::SaveWorkspaceSession { .. } => "SaveWorkspaceSession",
            Self::DeleteWorkspaceSession { .. } => "DeleteWorkspaceSession",
            Self::ActivateWorkspaceSession { .. } => "ActivateWorkspaceSession",
            Self::RequestWorkspaceGitStatus { .. } => "RequestWorkspaceGitStatus",
            Self::RequestWorkspaceFiles { .. } => "RequestWorkspaceFiles",
            Self::RequestBrowseDirectories { .. } => "RequestBrowseDirectories",
            Self::StartAllInstances { .. } => "StartAllInstances",
            Self::StopAllInstances { .. } => "StopAllInstances",
            Self::RequestSkillsList { .. } => "RequestSkillsList",
            Self::ReloadSkills { .. } => "ReloadSkills",
            Self::RequestToolsList { .. } => "RequestToolsList",
            Self::SaveFederationPeer { .. } => "SaveFederationPeer",
            Self::DeleteFederationPeer { .. } => "DeleteFederationPeer",
            Self::RequestFederationStatus { .. } => "RequestFederationStatus",
            Self::RequestFederationInvite { .. } => "RequestFederationInvite",
            Self::MigrateSession { .. } => "MigrateSession",
            Self::RequestSelfUpdate { .. } => "RequestSelfUpdate",
            Self::RequestSelfUpdateStatus { .. } => "RequestSelfUpdateStatus",
        }
    }

    /// 命令的**载荷**（自由 JSON；剥掉外层 tag）。
    ///
    /// 与 [`Self::kind_name`] 配合，把"已反序列化的命令"降级成"名字 + 载荷"
    /// 交给插件——插件侧可以再用 `serde_json::from_value::<BackendCommand>`
    /// 拿回强类型（它依赖 `echo-protocol`，这正是共享词汇的正确位置）。
    ///
    /// **单元变体**（无字段命令，如 `RequestPluginsList`）序列化成裸字符串
    /// `"RequestPluginsList"`，没有载荷 → 返回 `Null`；
    /// **结构体变体**剥掉外层 tag，返回该变体自己的字段对象。
    ///
    /// 失败（不可能发生：命令刚反序列化过）时也返回 `Null`。
    pub fn payload_value(&self) -> serde_json::Value {
        let name = self.kind_name();
        match serde_json::to_value(self) {
            Ok(serde_json::Value::Object(mut map)) => {
                map.remove(name).unwrap_or(serde_json::Value::Null)
            }
            // 单元变体（裸字符串）/ 其它形状：没有可剥的载荷。
            Ok(_) | Err(_) => serde_json::Value::Null,
        }
    }
}

/// 多上游聚合下**适合广播**的幂等只读命令名集合（参考清单）。
///
/// 历史（准确性校准 2026-10 巡检）：中继曾按命令名白名单判别"能否无壳
/// 广播"，后已改为**纯意图透传**——投递范围由客户端显式声明
/// （`{"core": <name>}` 定向 / `{"broadcast": true}` 广播 / 无壳仅单上游
/// 直发），中继不做命令名分类，原镜像列表与其一致性测试均已移除。
/// 本清单保留价值：定义"哪些命令是幂等只读、可安全广播"的权威参考
/// （前端引导命令即从此类命令中选取），无跨仓强制同步义务；
/// 契约快照测试见本文件 `broadcast_readonly_contract_snapshot`。
pub const BROADCAST_READONLY_COMMANDS: &[&str] = &[
    // 前端引导命令组（顺序同 Panel 的 onopen）
    "RequestState",
    "RequestTeamsList",
    "RequestInstanceStatus",
    "RequestShellSessions",
    "RequestTrunkTimeline",
    // 清单 / 发现类
    "RequestPluginsList",
    "RequestSkillsList",
    "RequestToolsList",
    "RequestSystemPrompt",
    "RequestRemoteApiProfiles",
    "RequestWorkspaceSessions",
    // 联邦与自更新
    "RequestFederationStatus",
    "RequestFederationInvite",
    "RequestSelfUpdateStatus",
];

/// 命令名是否属于可广播只读集合（中继白名单的权威定义）。
pub fn is_broadcast_readonly(command_name: &str) -> bool {
    BROADCAST_READONLY_COMMANDS.contains(&command_name)
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
/// **类别判定（W2.3，2026-10）**：插件实例管理命令按**类别**判定——
/// 一切 [`BackendCommand::InvokeInstanceOp`] 都是 Frontend-only：人在环
/// 配置面，模型绝不可控。类别判定取代了「逐平台变体枚举」——新增管理
/// 方法不再需要记得加枚举臂（消除「忘加枚举臂」回归窗口，fail-closed）。
pub fn command_clearance(cmd: &BackendCommand) -> CommandClearance {
    match cmd {
        // 插件实例管理命令：类别判定（任意平台、任意方法）。
        BackendCommand::InvokeInstanceOp { .. }
        | BackendCommand::SaveWorkspaceSession { .. }
        | BackendCommand::DeleteWorkspaceSession { .. }
        | BackendCommand::ActivateWorkspaceSession { .. }
        | BackendCommand::SaveFederationPeer { .. }
        | BackendCommand::DeleteFederationPeer { .. }
        | BackendCommand::RequestFederationStatus
        | BackendCommand::RequestFederationInvite
        | BackendCommand::MigrateSession { .. }
        | BackendCommand::RequestSelfUpdate
        | BackendCommand::RequestSelfUpdateStatus
        | BackendCommand::ClearHistory { .. } => CommandClearance::Frontend,
        _ => CommandClearance::Agent,
    }
}

fn default_true_agent() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 集合里的每个名字都必须真的是一个 `BackendCommand` 变体。
    ///
    /// 回归背景（2026-10 审计发现）：清单里曾长期躺着 `"Ping"`——`BackendCommand` 没有这个
    /// 变体，也没有任何发送方（Panel 的探活走 WS 层 Ping 控制帧，不经命令通道）。它留在
    /// 「权威参考」里会误导读者以为协议有探活命令。快照测试只比对字面量列表，抓不到这种
    /// 「名字根本不是命令」的情况——补充这条不变量。
    #[test]
    fn every_broadcast_readonly_name_is_a_real_command_variant() {
        // serde 的 unknown-variant 错误里带着全部合法变体名（"expected one of ..."）。
        let error = serde_json::from_str::<BackendCommand>(r#""__not_a_command__""#)
            .expect_err("unknown command must fail to deserialize")
            .to_string();
        // 末尾会带上 "` at line 1 column 7"，先切掉，否则最后一个变体名会被污染。
        let list = error
            .split("expected one of ")
            .nth(1)
            .unwrap_or_default()
            .split("` at line")
            .next()
            .unwrap_or_default();
        let variants: Vec<String> = list
            .split(',')
            .map(|s| s.trim().trim_matches('`').to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert!(
            variants.len() > 50,
            "变体名单解析失败（解析到 {} 个）：{error}",
            variants.len()
        );
        for name in super::BROADCAST_READONLY_COMMANDS {
            assert!(
                variants.iter().any(|v| v == name),
                "BROADCAST_READONLY_COMMANDS 里的 `{name}` 不是 BackendCommand 的变体"
            );
        }
    }

    /// 可广播只读集合的契约快照：改动必须与中继 `is_readonly_command` 同步。
    #[test]
    fn broadcast_readonly_contract_snapshot() {
        let mut names = super::BROADCAST_READONLY_COMMANDS.to_vec();
        names.sort();
        assert_eq!(
            names,
            vec![
                "RequestFederationInvite",
                "RequestFederationStatus",
                "RequestInstanceStatus",
                "RequestPluginsList",
                "RequestRemoteApiProfiles",
                "RequestSelfUpdateStatus",
                "RequestShellSessions",
                "RequestSkillsList",
                "RequestState",
                "RequestSystemPrompt",
                "RequestTeamsList",
                "RequestToolsList",
                "RequestTrunkTimeline",
                "RequestWorkspaceSessions",
            ]
        );
        assert!(super::is_broadcast_readonly("RequestState"));
        assert!(!super::is_broadcast_readonly("SendMessage"));
    }

    /// 插件实例管理命令的**类别判定**（W2.3）：`InvokeInstanceOp` 一律
    /// Frontend-only——不依赖逐变体枚举（新增方法零维护）。
    #[test]
    fn instance_management_commands_are_frontend_only_by_category() {
        for (method, payload) in [
            ("set_gate_mode", serde_json::json!({"mode": "allowlist"})),
            ("update_allowlist", serde_json::json!({"user_ids": [1]})),
            ("set_owner", serde_json::json!({"owner_qq": 123})),
            ("login_qrcode", serde_json::json!({})),
            ("future_unknown_method", serde_json::json!({})),
        ] {
            assert_eq!(
                command_clearance(&BackendCommand::InvokeInstanceOp {
                    instance: "qq".into(),
                    method: method.into(),
                    payload,
                }),
                CommandClearance::Frontend,
                "adapter op {method} must be frontend-only"
            );
        }
    }

    /// `RenameApi` 线格式：字段名必须与 Panel 手工镜像一致
    /// （`{"RenameApi":{"from":"a","to":"b"}}`；Panel protocol-sync 契约）。
    #[test]
    fn rename_api_serde_contract() {
        let cmd = BackendCommand::RenameApi {
            from: "kimi".into(),
            to: "k3".into(),
        };
        let text = serde_json::to_string(&cmd).unwrap();
        assert_eq!(text, r#"{"RenameApi":{"from":"kimi","to":"k3"}}"#);
        let back: BackendCommand = serde_json::from_str(&text).unwrap();
        assert_eq!(back, cmd);
    }

    /// `RequestSessionHistory` 线格式（2026-10 会话显示历史通道）：字段名与
    /// 序列化顺序固定（Panel 手工镜像协议；team_id / limit 可省略或为 null）。
    #[test]
    fn request_session_history_serde_contract() {
        let cmd = BackendCommand::RequestSessionHistory {
            team_id: Some("t".into()),
            session_id: "s".into(),
            limit: Some(100),
        };
        let text = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            text,
            r#"{"RequestSessionHistory":{"team_id":"t","session_id":"s","limit":100}}"#
        );
        let back: BackendCommand = serde_json::from_str(&text).unwrap();
        assert_eq!(back, cmd);

        // 缺省形态：team_id / limit 可整体省略（serde default），也可为 null。
        let bare: BackendCommand =
            serde_json::from_str(r#"{"RequestSessionHistory":{"session_id":"s"}}"#).unwrap();
        assert_eq!(
            bare,
            BackendCommand::RequestSessionHistory {
                team_id: None,
                session_id: "s".into(),
                limit: None,
            }
        );
        let null: BackendCommand = serde_json::from_str(
            r#"{"RequestSessionHistory":{"team_id":null,"session_id":"s","limit":null}}"#,
        )
        .unwrap();
        assert_eq!(null, bare);
    }
    /// `kind_name()` 与 serde 的线上 tag 逐字相同（插件按名字订阅，分叉即失配）。
    #[test]
    fn kind_name_matches_the_serde_tag() {
        let samples = [
            BackendCommand::RequestPluginsList,
            BackendCommand::RestartPlugin { id: "p".into() },
            BackendCommand::TogglePlugin {
                id: "p".into(),
                enabled: true,
            },
            BackendCommand::RequestWorkspaceFiles {
                session_id: "s".into(),
                team_id: None,
                path: String::new(),
            },
        ];
        for cmd in &samples {
            let value = serde_json::to_value(cmd).expect("serialize");
            // 外部标签枚举：结构体变体 → `{"Name": {...}}`；单元变体 → `"Name"`。
            let tag = match &value {
                serde_json::Value::Object(map) => {
                    map.keys().next().cloned().expect("结构体变体应只有一个键")
                }
                serde_json::Value::String(name) => name.clone(),
                other => panic!("意外的序列化形状: {other:?}"),
            };
            assert_eq!(&tag, cmd.kind_name(), "kind_name 与 serde tag 分叉");
        }
    }

    /// `payload_value()` 剥掉外层 tag：插件拿到的就是命令自己的字段。
    #[test]
    fn payload_value_unwraps_the_tag() {
        let cmd = BackendCommand::RestartPlugin {
            id: "echo-agent.x".into(),
        };
        let payload = cmd.payload_value();
        assert_eq!(payload["id"], "echo-agent.x");
        assert!(payload.get("RestartPlugin").is_none(), "外层 tag 应已剥掉");

        // 无字段命令 → 载荷为 Null（不是缺键，也不是空对象）
        assert_eq!(
            BackendCommand::RequestPluginsList.payload_value(),
            serde_json::Value::Null
        );
    }
}
