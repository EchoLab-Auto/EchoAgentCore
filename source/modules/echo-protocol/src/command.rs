//! Commands sent by the TUI / API layer into the agent backend.

use echo_defs::mode::{ReasoningEffort, ThinkingMode};

/// 面板随消息上传的文件（字节内嵌）。
///
/// ## 为什么字节内嵌，而不是「先上传、再传引用」
///
/// Core **只有 ws 一个入口**（`agent-module-management` 在裸 TcpListener 上
/// 升级 websocket，没有 HTTP 服务），所以没有现成的上传端点；更要紧的是
/// federation 下 Panel 与 Core 可能不在一台机器上，「先落盘再传路径」需要
/// 一个两端都能访问的中转目录，跨机时它不存在。
///
/// 落盘位置与命名规则由 Core 单方面决定（`echo_defs::attachment_store`）：
/// 前端只提供原文件名与字节，不参与路径决策——否则同一份协议要在两个仓库
/// 各维护一套目录约定。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
pub struct PendingFile {
    /// 用户看到的文件名（`report.pdf`）。Core 落盘前消毒，不可信。
    pub name: String,
    /// base64 字节。也接受 `data:<mime>;base64,` 前缀（浏览器 FileReader 的
    /// 原生输出就是那种形状，前端不必先剥）。
    pub data: String,
}

/// Commands from the frontend to the agent backend.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
pub enum BackendCommand {
    /// Send a message into a session (as if from the local user).
    /// `images` carries image URLs / data URIs for multimodal models.
    /// `team_id` routes the message to a team member (None = default).
    SendMessage {
        session_id: String,
        content: String,
        #[serde(default)]
        images: Vec<String>,
        /// 面板随消息拖入的文件（字节内嵌，Core 负责落盘并告知 agent 路径）。
        #[serde(default)]
        files: Vec<PendingFile>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
    },
    /// Switch the active model.
    SwitchModel {
        model: String,
    },
    /// Switch the active provider.
    SwitchProvider {
        provider: String,
    },
    /// Replace the system prompt (owned by the Core module, not the API config).
    SetSystemPrompt {
        prompt: String,
    },
    /// Request the current system prompt module text.
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
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        thinking: Option<ThinkingMode>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        reasoning_effort: Option<ReasoningEffort>,
    },
    /// Switch to an API profile by name (empty = top-level default).
    SwitchApi {
        name: String,
    },
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
    RequestRemoteApiProfiles {
        peer: String,
    },
    /// Test connectivity of an API config: `name` empty = top-level default,
    /// otherwise that profile. Replies with `BackendEvent::ApiTestResult`.
    TestApi {
        name: String,
    },
    /// Query account balance of an API config (DeepSeek endpoint only):
    /// `name` empty = top-level default, otherwise that profile.
    /// Replies with `BackendEvent::ApiBalanceResult`.
    QueryApiBalance {
        name: String,
    },
    /// 请求 API profile 的**本地积累指标**（余额快照 + token 用量时间序列）：
    /// `name` 空 = 默认配置 + 全部 profile；非空 = 指定 profile。
    /// 回 `BackendEvent::ApiMetrics`（单事件，每个请求目标一条 entry）。
    QueryApiMetrics {
        name: String,
    },
    /// Delete an API profile by name.
    DeleteApi {
        name: String,
    },
    /// 原子改名 API profile：重写全部按名引用（供应商池条目名 / `active_api` /
    /// `teams[*].api_profile`），并重绑运行中的 persona 引用、会话覆盖与
    /// 工作区对话引用。`to` 去空白后必须非空且不与现有名字冲突；`from`
    /// 必须精确存在。成功回 `ApiConfigUpdated` + `ApiProfilesUpdated`
    /// （teams 有变更时补 `TeamsList`）；失败回 `Error` 且零写入。
    RenameApi {
        from: String,
        to: String,
    },
    // ---- ChatGPT 订阅登录（2026-11：设置页直接发起，不再依赖 CLI）----
    //
    // 三条命令都归 `agent-module-provider-llm`（它拥有 chatgpt 模块）。
    // 为什么做成命令而不是"面板去跑 CLI"：Core 可能跑在另一台机器 / 容器里，
    // 面板碰不到它的终端；而 OAuth 回调必须打到 **Core 那台** 的 1455 端口。
    // 命令面是唯一能让"用户在浏览器点、Core 在后台收"闭环的地方。
    /// 开始一次 ChatGPT 订阅登录。
    ///
    /// 进度经 `ChatGptLoginPending`（授权 URL）/ `ChatGptLoginResult`
    /// （成功或失败）回推。已有登录在进行时回 `Error`，不静默排队——
    /// 两次登录抢同一个 1455 端口，排队只会让第二个必然失败且原因隐晦。
    StartChatGptLogin,
    /// 取消进行中的登录（释放 1455 端口；面板关掉弹层时用）。
    CancelChatGptLogin,
    /// 手工提交授权码（**局域网 / 远程部署的主要路径**）。
    ///
    /// 回调固定打 `localhost:1455`，而 `localhost` 是**浏览器那台机器**。
    /// 用户从另一台机器登录时，浏览器会跳到不存在服务的 1455——但地址栏里
    /// 已经带着 `code` 与 `state`。把那条 URL 整条粘回来即可完成登录：
    /// 不需要 SSH 隧道、不需要改监听地址。
    ///
    /// `input` 接受完整 URL / 只有 query / 只有 code 三种形态（见
    /// `chatgpt::auth::extract_code_from_input`）。
    SubmitChatGptLoginCode {
        input: String,
    },
    /// 读登录态（是否已登录 / 账户邮箱 / 过期时刻），回 `ChatGptStatus`。
    ///
    /// 只读：面板进设置页时拉一次，用来决定显示「登录」还是「已登录」。
    RequestChatGptStatus,
    /// Request a full state snapshot（逐会话回 `BackendEvent::SessionUpdated`
    /// 等事件流，不发状态结构体）。
    RequestState,
    /// Enable or disable a skill.
    ToggleSkill {
        name: String,
        enabled: bool,
    },
    /// Enable or disable a registered tool at runtime. Disabled tools are
    /// hidden from the model and calls fail; persisted via `[agent]`.
    ToggleTool {
        name: String,
        enabled: bool,
    },
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
    DeleteSkill {
        name: String,
    },
    /// 归档某个 team 的历史会话（快照到 archives/ 并清空当前历史）。
    ///
    /// `session_id` 存在时**会话级**：只归档并清空该会话（2026-10 审计
    /// 修复——此前无会话维度，点一次误清同人格全部会话）；缺失时保持
    /// 旧语义（整档归档 + 全量清空），向后兼容旧面板。
    ArchiveHistory {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        session_id: Option<String>,
    },
    /// 压缩指定会话的历史：旧事件替换为摘要（保留最近 keep_recent 条）。
    /// `session_id` 必填；缺失时服务端拒绝，避免误压该人格的全部会话。
    CompactHistory {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        session_id: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        keep_recent: Option<usize>,
    },
    /// Request the list of all mounted modules.
    /// Responds with `BackendEvent::ModulesList`.
    RequestModulesList,
    /// Request the list of team members (multi-agent).
    /// Responds with `BackendEvent::TeamsList`.
    RequestTeamsList,
    /// Enable or disable a team member at runtime (unloads memory/logs).
    ToggleTeam {
        id: String,
        enabled: bool,
    },
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
        enabled_modules: Vec<String>,
        #[serde(default)]
        enabled_tools: Vec<String>,
        #[serde(default)]
        enabled_skills: Vec<String>,
        /// Per-agent trunk token budget (None = 继承全局 [agent].memory_limit_tokens)。
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        memory_limit_tokens: Option<usize>,
        /// Per-agent context window cap (None = 继承全局 [agent].context_window_tokens)。
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        context_window_tokens: Option<usize>,
        /// Persona 级 API 供应商引用（None = 跟随全局默认配置；
        /// Some(name) = 使用全局供应商池 `[agent].api_profiles` 中的该 profile）。
        /// 保存在 `[agent.teams.{id}].api_profile`，运行期即时重建该 persona 的 provider。
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        api_profile: Option<String>,
    },
    /// Delete a team member (the default/main agent is protected).
    DeleteTeam {
        id: String,
    },
    /// 重载一个模块（卸下 → 重新挂载），**不改**启用状态与持久化配置。
    ///
    /// 用途：模块按新配置/新外部依赖重新初始化（子进程模块重连、实例
    /// 模块重启进程）；管理面模块被拒绝（重启 = Panel 断连）。
    RestartModule {
        id: String,
    },
    /// Request a snapshot of a conversation context (`BackendEvent::ContextSnapshot`).
    ///
    /// 多会话（2026-09）：`session_id` 指定要查看的会话上下文；缺省回退
    /// 该智能体的本地 TUI 会话（再回退任一已有会话）。
    RequestContext {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
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
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
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
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        name: Option<String>,
        /// 分支/标签（缺省 = 仓库默认分支）
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        branch: Option<String>,
    },
    /// 更新从 Git 安装的 skill（根据 sources 记录 fetch + reset）。
    UpdateSkillFromGit {
        name: String,
    },
    /// 移除 Git 来源记录（不删除目录；目录删除仍用 DeleteSkill）。
    RemoveSkillSource {
        name: String,
    },
    RequestTrunkTimeline {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
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
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
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
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
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
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        limit: Option<usize>,
    },
    /// 请求某会话的**显示历史**（从事件日志按会话派生；2026-10 修复面板
    /// 「会话无消息」——共享显示时间线按 FIFO 淘汰后旧会话条目不可见）。
    /// `limit`：最多返回条目数（缺省 500；服务端 clamp [1, 2000]，取最近 N 条）。
    ///
    /// `expand_depth`（2026-11）：把**压缩前归档**接进同一条消息流，展开
    /// 几层（`None`/`0` = 不展开，只标 `has_older`）。
    ///
    /// 压缩把日志前缀换成一条摘要、原件写进 `archives/`，那段历史在投影里
    /// 永久消失——不展开的话面板只能看到一个"上下文已压缩"的断点。
    ///
    /// 为什么是「层数」而不是布尔：压缩链实测可达 6 层（`echo-agent/v`
    /// 2026-11 就是 6 层、合计约 125 MB），每层要一次 20+ MB 的 JSON 解析。
    /// 面板每点一次「展开更早的」把层数 +1，服务端只解析**这一次新增**的
    /// 那层（浅层命中缓存）。
    ///
    /// 展开时**忽略 `limit`**（一次给完整层，否则用户要点十几次才跨过压缩
    /// 边界），仅受服务端硬上限约束。
    RequestSessionHistory {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        session_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        limit: Option<usize>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        expand_depth: Option<usize>,
    },
    /// 请求后台 shell 会话列表（带 team_id 时只返回该 persona 的会话）。
    RequestShellSessions {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
    },
    /// 新建一个后台 shell 会话（持久 bash，可反复执行命令）。
    ShellStart {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        workdir: Option<String>,
    },
    /// 在指定 shell 会话中执行一条命令（超时内返回输出；超时标注且会话保留）。
    ShellExec {
        session_id: String,
        command: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        timeout_secs: Option<u64>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
    },
    /// 停止（销毁）一个后台 shell 会话。
    ShellStop {
        session_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
    },
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
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        session_id: Option<String>,
    },
    /// Start an instance by name.
    StartInstance {
        name: String,
    },
    /// Stop an instance by name.
    StopInstance {
        name: String,
    },
    /// Restart an instance.
    RestartInstance {
        name: String,
    },
    /// Request the current instance status list.
    RequestInstanceStatus,
    /// 调用一个模块实例管理方法（W2.1，2026-10 模块实例管理通道）。
    ///
    /// **唯一新增的模块实例管理命令**——取代逐平台专属命令（旧 QQ 专属变体
    /// 已于 2026-10 直接删除）。
    /// `method` 与模块协议 `custom op` 及 `InstanceManagement::invoke_custom`
    /// 同名直通（如 `set_gate_mode` / `update_allowlist` / `login_qrcode`）。
    /// 响应为 [`crate::event::BackendEvent::InstanceOpResult`]。
    ///
    /// **Frontend-only**（类别判定，fail-closed）：一切模块实例管理命令都是
    /// 人在环配置面，模型绝不可控。
    InvokeInstanceOp {
        /// 模块实例名（旧线协议字段名 `adapter` 仍接受——2026-10 去模块实例化）。
        #[serde(alias = "adapter")]
        instance: String,
        /// 管理方法名（平台自定义；推荐名见 `InstanceManagement` 文档）。
        method: String,
        /// 方法参数（JSON；缺省空对象）。
        #[serde(default)]
        payload: serde_json::Value,
    },
    /// 调用**任意已挂载模块**自己声明的一个方法（2026-11 通用模块调用通道）。
    ///
    /// 与 [`Self::InvokeInstanceOp`] 的关系：那条是"模块实例的管理面"（QQ 之类
    /// 平台自定义方法），这条是"模块的能力面"——**宿主完全不解释 `method` 与
    /// `payload`**，只按 `module` 找到模块、把调用送进它的 `Contribution::Command`
    /// 分发表，再把返回值原样回给调用方。
    ///
    /// 为什么必须有这条：本通道之前，任何模块想给面板提供数据，都只有"在
    /// `BackendCommand` 里加一个具名变体"这一条路——那等于把模块的业务词汇
    /// 钉进 Core 的协议（前端生成的类型里也就跟着出现该模块的概念）。有了
    /// 通用调用，模块自己的读接口不再需要宿主认识它。
    ///
    /// 响应为 [`crate::event::BackendEvent::ModuleOpResult`]（**必发**：模块
    /// 未挂载 / 未声明该 method / 调用失败都回显式错误，绝不让面板等成超时）。
    ///
    /// **Frontend-only**（类别判定，fail-closed）：通用通道等于"任意模块的
    /// 任意方法"，模型不可控它——否则一个被投毒的模块描述就能让模型驱动
    /// 任意模块的管理面。
    InvokeModuleOp {
        /// 目标模块 id（`ModuleInfo.id`，如 `vendor.group.name`）。
        module: String,
        /// 方法名：目标模块在 `contribution` 里声明的命令名。
        method: String,
        /// 方法参数（JSON；缺省空对象）。模块自行约定形状。
        #[serde(default)]
        payload: serde_json::Value,
    },
    // ---- 工作区会话（workspace 模块：基于工作空间的会话管理）----
    /// 请求某 persona 的工作区会话列表。
    /// 响应 `BackendEvent::WorkspaceSessions`。
    RequestWorkspaceSessions {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
    },
    /// 新建/更新一个工作区会话（按 `session.id` upsert；空 id 由服务端
    /// 依名称生成）。响应为刷新后的 `WorkspaceSessions` 列表。
    SaveWorkspaceSession {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        session: crate::event::WorkspaceSessionInfo,
    },
    /// 删除一个工作区会话（若为激活会话则同时清除激活标记）。
    DeleteWorkspaceSession {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        id: String,
    },
    /// 激活（或 `id = None` 取消激活）一个工作区会话。
    /// 激活会话的工作目录会注入系统提示词。
    ActivateWorkspaceSession {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        id: Option<String>,
        /// 工作区内的对话 id（2026-10 多对话；None = 进入该工作区的
        /// `last_conversation` / 首条对话）。`id = None`（取消激活）时忽略。
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        conversation: Option<String>,
    },
    /// 请求某会话各工作区目录的 git 状态（`git` CLI 采集，只读）。
    /// 响应 `BackendEvent::WorkspaceGitStatus`。
    /// 注意：这里的 `session_id`（与 [`Self::RequestWorkspaceFiles`] 同）
    /// 实际承载的是**工作区 id**（`WorkspaceStore` 的键），不是对话会话 id
    /// ——历史遗留命名。真正的对话会话是 `local:workspace:<ws>/<conv>:…`
    /// 那种派生通道 id（2026-10 作用域普查确认，改名会破坏线协议，故只此注记）。
    RequestWorkspaceGitStatus {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        session_id: String,
    },
    /// 请求列出某工作区目录下的文件与子目录（文件浏览器，只读）。
    /// `path` 必须是该会话某个工作区目录本身或其后代（服务端以
    /// canonical 路径前缀校验，越界拒绝）。响应 `BackendEvent::WorkspaceFiles`。
    /// `session_id` 同上（实为工作区 id，非对话会话 id）。
    RequestWorkspaceFiles {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
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
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        path: Option<String>,
    },

    // ---- ProDoc 文档群（prodoc 模块：把 Markdown 目录读成文档图）----
    /// 扫描一个文档根并**推回**全量快照。
    ///
    /// 回执只有统计（`{root, documents, bytes, truncated}`），**内容走模块
    /// 事件**（`ModuleEvent{name:"prodoc.bundle"}`）：一份快照可能几百 KB，
    /// 塞进命令回执会和面板的 pending / 重放逻辑缠在一起，而模块事件通道
    /// 本来就是为"服务器主动推"设计的。
    ///
    /// `root` 必须是模块配置 `roots` 允许的范围内（缺省只有 HOME）。
    RequestProDocBundle {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        root: String,
    },
    /// 目录选择器的逐层浏览：`path = None` 给根列表，给路径给子目录。
    BrowseProDocDirs {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        path: Option<String>,
    },
    /// 可打开的根列表 + 生效的上限（面板初次连接时拉一次）。
    ProDocRoots,
    /// 读单个文档的磁盘原文（冲突面板并排对照 + 合并基准）。
    /// 响应 `ModuleEvent{name:"prodoc.file"}`。
    ReadProDocFile {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        root: String,
        path: String,
        /// 面板生成的回执关联 id（模块原样带回）。
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        request_id: Option<String>,
    },
    /// 保存单个文档（带 `base` 冲突检测）。
    ///
    /// `base` = 客户端依据的磁盘内容；缺省 = 无条件覆盖（新建文件走这条）。
    /// 冲突时回执带 `disk`（磁盘当前内容），前端据此跑 diff3 后以
    /// `base = disk` 重发——**合并留在前端**：CLI 用的是
    /// `prodoc-core/merge.ts` 的 diff3，在 Rust 复刻保证不了语义一致。
    SaveProDocFile {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        root: String,
        path: String,
        content: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        base: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        request_id: Option<String>,
    },
    /// 删除单个文档（`base` 不匹配则取消删除）。
    DeleteProDocFile {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        root: String,
        path: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        base: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        request_id: Option<String>,
    },
    /// 文档图检查（断链 / 自身连线 / 重复 id / 环 / 孤立 / 空正文 / 超长 /
    /// 缺参数区 / 单轴坐标）。
    ///
    /// 报告走 `ModuleEvent{name:"prodoc.report"}`（含统计与逐条 issue）；
    /// 回执只给计数。**检查有问题不等于命令失败**：文档可以有意留草稿。
    ProDocCheck {
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        root: String,
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
    // ── 自更新（2026-10，**Frontend-only**）──
    /// 触发框架自更新：启动 `echo-agent-core-update.service`（oneshot，
    /// 脱离本进程执行 构建→替换→重启）。Graceful Drain 保证当前回复
    /// 完整生成后再重启；面板短暂重连后会话自动恢复。响应为
    /// `SelfUpdateStatus` 事件（状态轮询由前端继续）。
    RequestSelfUpdate,
    /// 查询自更新状态（读 `update-status` 文件 → `SelfUpdateStatus`）。
    RequestSelfUpdateStatus,

    // ── 联邦（Frontend-only；实现归 agent-module-federation）──
    RequestFederationStatus,
    RequestFederationDirectory,
    SaveFederationPeer {
        peer: crate::federation::FederationPeerInfo,
    },
    DeleteFederationPeer {
        name: String,
    },
    RequestFederationInvite,
    MigrateSession {
        session_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts-bindings", ts(optional = nullable))]
        team_id: Option<String>,
        target_peer: String,
    },
}

impl BackendCommand {
    /// 变体名的**零分配**取值（等于 serde 的线上 tag）。
    ///
    /// `BackendCommand` 没有 `rename_all`，所以线上 tag 就是 Rust 变体名——
    /// 本函数返回的字符串与 `serde_json::to_value(&cmd)` 的第一个键逐字相同
    /// （有测试守着这条等价关系）。
    ///
    /// 用途：命令分发的**模块前置查表**。模块按名字声明"我处理哪些命令"
    /// （`Contribution::Command`），宿主需要"不序列化就看得到名字"——
    /// `apply_command` 拿到的是已反序列化的枚举，为拿名字再 `to_value` 一遍
    /// 是纯浪费（而命令里有大载荷，比如整段消息）。
    ///
    /// **穷举 match，没有 `_` 分支**：新增命令变体时编译器会逼你在这里起名。
    /// 忘了起名会让模块永远收不到该命令，而这种静默失败比编译错误难查得多。
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
            Self::StartChatGptLogin => "StartChatGptLogin",
            Self::CancelChatGptLogin => "CancelChatGptLogin",
            Self::SubmitChatGptLoginCode { .. } => "SubmitChatGptLoginCode",
            Self::RequestChatGptStatus => "RequestChatGptStatus",
            Self::RequestState { .. } => "RequestState",
            Self::ToggleSkill { .. } => "ToggleSkill",
            Self::ToggleTool { .. } => "ToggleTool",
            Self::SaveSkill { .. } => "SaveSkill",
            Self::DeleteSkill { .. } => "DeleteSkill",
            Self::ArchiveHistory { .. } => "ArchiveHistory",
            Self::CompactHistory { .. } => "CompactHistory",
            Self::RequestModulesList { .. } => "RequestModulesList",
            Self::RequestTeamsList { .. } => "RequestTeamsList",
            Self::ToggleTeam { .. } => "ToggleTeam",
            Self::SaveTeam { .. } => "SaveTeam",
            Self::DeleteTeam { .. } => "DeleteTeam",
            Self::RestartModule { .. } => "RestartModule",
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
            Self::InvokeModuleOp { .. } => "InvokeModuleOp",
            Self::RequestWorkspaceSessions { .. } => "RequestWorkspaceSessions",
            Self::SaveWorkspaceSession { .. } => "SaveWorkspaceSession",
            Self::DeleteWorkspaceSession { .. } => "DeleteWorkspaceSession",
            Self::ActivateWorkspaceSession { .. } => "ActivateWorkspaceSession",
            Self::RequestWorkspaceGitStatus { .. } => "RequestWorkspaceGitStatus",
            Self::RequestWorkspaceFiles { .. } => "RequestWorkspaceFiles",
            Self::RequestBrowseDirectories { .. } => "RequestBrowseDirectories",
            Self::RequestProDocBundle { .. } => "RequestProDocBundle",
            Self::BrowseProDocDirs { .. } => "BrowseProDocDirs",
            Self::ProDocRoots { .. } => "ProDocRoots",
            Self::ReadProDocFile { .. } => "ReadProDocFile",
            Self::SaveProDocFile { .. } => "SaveProDocFile",
            Self::DeleteProDocFile { .. } => "DeleteProDocFile",
            Self::ProDocCheck { .. } => "ProDocCheck",
            Self::StartAllInstances { .. } => "StartAllInstances",
            Self::StopAllInstances { .. } => "StopAllInstances",
            Self::RequestSkillsList { .. } => "RequestSkillsList",
            Self::ReloadSkills { .. } => "ReloadSkills",
            Self::RequestToolsList { .. } => "RequestToolsList",
            Self::SaveFederationPeer { .. } => "SaveFederationPeer",
            Self::DeleteFederationPeer { .. } => "DeleteFederationPeer",
            Self::RequestFederationStatus { .. } => "RequestFederationStatus",
            Self::RequestFederationDirectory { .. } => "RequestFederationDirectory",
            Self::RequestFederationInvite { .. } => "RequestFederationInvite",
            Self::MigrateSession { .. } => "MigrateSession",
            Self::RequestSelfUpdate { .. } => "RequestSelfUpdate",
            Self::RequestSelfUpdateStatus { .. } => "RequestSelfUpdateStatus",
        }
    }

    /// 命令的**载荷**（自由 JSON；剥掉外层 tag）。
    ///
    /// 与 [`Self::kind_name`] 配合，把"已反序列化的命令"降级成"名字 + 载荷"
    /// 交给模块——模块侧可以再用 `serde_json::from_value::<BackendCommand>`
    /// 拿回强类型（它依赖 `agent-module-management`，这正是共享词汇的正确位置）。
    ///
    /// **单元变体**（无字段命令，如 `RequestModulesList`）序列化成裸字符串
    /// `"RequestModulesList"`，没有载荷 → 返回 `Null`；
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

/// `serde(default = ...)` 的缺省值：该字段历史缺省为 `true`。
fn default_true_agent() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
            expand_depth: None,
        };
        let text = serde_json::to_string(&cmd).unwrap();
        // `expand_depth: None` 序列化为 null（面板镜像时按"显式传"处理）。
        assert_eq!(
            text,
            r#"{"RequestSessionHistory":{"team_id":"t","session_id":"s","limit":100,"expand_depth":null}}"#
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
                expand_depth: None,
            }
        );
        let null: BackendCommand = serde_json::from_str(
            r#"{"RequestSessionHistory":{"team_id":null,"session_id":"s","limit":null}}"#,
        )
        .unwrap();
        assert_eq!(null, bare);
    }

    /// `kind_name()` 与 serde 的线上 tag 逐字相同（模块按名字订阅，分叉即失配）。
    #[test]
    fn kind_name_matches_the_serde_tag() {
        let samples = [
            BackendCommand::RequestModulesList,
            BackendCommand::RestartModule { id: "p".into() },
            BackendCommand::RequestWorkspaceFiles {
                session_id: "s".into(),
                team_id: None,
                path: String::new(),
            },
            BackendCommand::InvokeModuleOp {
                module: "echo-agent.checklist".into(),
                method: "checklist.snapshot".into(),
                payload: serde_json::json!({"team_id": "alix"}),
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

    /// `payload_value()` 剥掉外层 tag：模块拿到的就是命令自己的字段。
    #[test]
    fn payload_value_unwraps_the_tag() {
        let cmd = BackendCommand::RestartModule {
            id: "echo-agent.x".into(),
        };
        let payload = cmd.payload_value();
        assert_eq!(payload["id"], "echo-agent.x");
        assert!(payload.get("RestartModule").is_none(), "外层 tag 应已剥掉");

        // 无字段命令 → 载荷为 Null（不是缺键，也不是空对象）
        assert_eq!(
            BackendCommand::RequestModulesList.payload_value(),
            serde_json::Value::Null
        );
    }
}
