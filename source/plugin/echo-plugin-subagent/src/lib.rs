//! # echo-plugin-subagent — Subagent 委派插件（解耦计划 Phase 3 ⑥）
//!
//! `spawn_subagent` 编排工具 + 隔离上下文的子 agent 循环，原
//! `echo-agent::packages::subagent` 的独立 crate 化。
//!
//! 主 agent 经 [`SpawnSubagentTool`] 拉起子 agent：子任务以**隔离上下文**运行
//! （自己的系统提示词与工具循环，不回写 trunk），完成后经结构化 hook
//! （`<subagent_event>`，见 [`wrap_subagent_event`]）以全新入站分支通知主
//! agent——hook 注入接口由 echo-loop 的 `SubagentToolHooks` 定义，QQ 消息等
//! 其他插件复用同一机制（入站 = 结构化 hook → 新 turn）。
//!
//! 模块构成：
//! - [`SubagentStore`]：运行中子任务注册表（取消句柄 + 任务快照，TTL 清扫）
//! - [`SpawnSubagentTool`]：模型可见工具（异步拉起，立即返回 id）
//! - [`run_subagent_turn`]：子 agent 的一次性 turn（独立于主循环，避免递归
//!   hook 注入）
//!
//! ## 依赖纪律与宿主接缝
//!
//! 只依赖定义层（`echo-defs`）与工具机制（`echo-tool`）——**不认识 agent
//! 框架，也不认识联邦实现**。跨模块需求一律经 trait 注入：
//!
//! - [`RemoteDelegationSink`]：远程委派的聚合登记（宿主把联邦聚合器接上；
//!   不注入 = 远程委派照常发出，只是不进父 turn 的聚合摘要）；
//! - `spawn` 执行闭包：由宿主注入（携带 provider / skills / 事件发射），
//!   因此本 crate 不持有 `Agent`，也不产生循环引用。
//!
//! 设计文档见 `document/plugins/core-subagent.md`。

pub mod execution;

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use echo_defs::llm::{
    LlmProvider, MAX_TRUNCATION_CONTINUES, TRUNCATION_CONTINUE_PROMPT, TURN_CANCELLED,
};
use echo_defs::message::{ChatMessage, ChatRequest, ChatRole};
use echo_defs::remote_tool::{remote_tool_name, remote_tool_prefix};
use echo_defs::token::truncate;
use echo_tool::{Tool, ToolError, ToolRegistry, ToolResult};

/// 推进回调（见 [`run_subagent_turn`] 的 `progress` 参数）。
///
/// `Arc` 而非引用：执行体的看门狗与 turn 本体分别持有，生命周期不同。
pub type ProgressFn = Arc<dyn Fn() + Send + Sync>;

/// 单次委派结果的可见回灌上限（字符）：子任务结论超过即截断并标注，
/// 防单个子任务反过来烧掉主上下文。
pub const MAX_SUBAGENT_RESULT_CHARS: usize = 8 * 1024;

/// 已完成任务条目在注册表中的保留时间（到点由清扫任务移除）。
const COMPLETED_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// 子 agent 允许的最大委派深度：子 agent 的 spawn 能力被剥离（深度 1），
/// 不存在递归委派；常量保留给未来显式深度放宽。
#[allow(dead_code)]
pub const MAX_DELEGATION_DEPTH: u32 = 1;

/// 子任务默认执行超时（秒）；`0` = 不设墙钟上限。
///
/// 这个值只是**兜底**：宿主编译期会用 `AgentConfig::subagent_timeout_secs`
/// 覆盖它（见 `SpawnSubagentTool::default_timeout_secs`）。慢模型（kimi 等
/// 长思考 / 多轮检索）不该被一个固定的 10 分钟砍掉，但"完全不管"也不行 ——
/// 见 [`SUBAGENT_IDLE_TIMEOUT_SECS`]：墙钟可以不设，卡死必须能被发现。
pub const DEFAULT_SUBAGENT_TIMEOUT_SECS: u64 = 600;

/// 显式指定墙钟上限时的允许区间（`0` 表示不限制，不参与 clamp）。
const MIN_SUBAGENT_TIMEOUT_SECS: u64 = 30;
const MAX_SUBAGENT_TIMEOUT_SECS: u64 = 86_400;

/// 「无进展」看门狗（秒）：子任务连续这么久没有任何推进即中止。
///
/// **为什么需要它**：墙钟上限与"慢"是两回事。一个每轮都在推进、只是每轮都
/// 慢的模型（长思考 + 十几轮工具）不应该被杀；而一个卡在挂死的 HTTP 请求上、
/// 或者陷入同一工具死循环的任务，无论给多久都完不成。这里用"有没有推进"作
/// 判据，两者就能分开：
/// - 推进 = 任一 LLM 返回（含工具调用）/ 任一工具调用完成（见 `run_subagent_turn`
///   的 `progress` 回调）；
/// - 空闲时长超过本常量 → 中止，回报里明确写"长时间无进展"而不是"超时"。
///
/// 30 分钟：单次 LLM 响应或单次工具调用远够不到这个量级（`bash` 工具自身上限
/// 300s，最长的推理也就在分钟级），所以它只会命中真正的卡死。
pub const SUBAGENT_IDLE_TIMEOUT_SECS: u64 = 1800;

/// 看门狗轮询间隔：只影响"发现卡死"的精度，不影响判据。
pub(crate) const IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// `spawn_subagent` 编排工具名（唯一事实来源）。
pub const SPAWN_SUBAGENT_TOOL: &str = "spawn_subagent";

/// `<subagent_event>` hook 包装（与 `<qq_message_hook>`/`<timer_event>` 同族）。
pub fn wrap_subagent_event(payload: &Value) -> String {
    format!(
        "<subagent_event>\n{}\n</subagent_event>",
        serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string())
    )
}

/// 运行中/刚完成的子任务快照（Panel 观测与取消寻址用）。
#[derive(Debug, Clone)]
pub struct SubagentInfo {
    pub id: String,
    pub task: String,
    pub session_id: String,
    pub started_at_ms: i64,
    pub status: SubagentStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// 本插件状态与**线协议**词汇（`echo_federation::SubagentStatus`）的对应。
///
/// 为什么两份而不是一份：`echo-federation` 是线协议 crate，插件不该为了一个
/// 状态枚举去依赖整条链路（它只在 `node` 委派时才用到）。映射放在插件里，
/// 于是宿主拿到的永远是协议词汇，不需要做转换。
pub fn wire_status(status: SubagentStatus) -> echo_federation::SubagentStatus {
    match status {
        SubagentStatus::Running => echo_federation::SubagentStatus::Running,
        SubagentStatus::Completed => echo_federation::SubagentStatus::Completed,
        SubagentStatus::Failed => echo_federation::SubagentStatus::Failed,
        SubagentStatus::Cancelled => echo_federation::SubagentStatus::Cancelled,
    }
}

impl SubagentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

struct SubagentEntry {
    info: SubagentInfo,
    cancel: tokio_util::sync::CancellationToken,
}

/// 子任务注册表：每个 persona 一份（组合根装配）。
///
/// 取消句柄让主 turn 取消可以传播到子 turn（`cancel_all`）；完成条目保留
/// TTL 供观测查询。后台清扫由 [`SubagentStore::spawn_sweeper`] 承担。
pub struct SubagentStore {
    entries: dashmap::DashMap<String, SubagentEntry>,
}

impl SubagentStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: dashmap::DashMap::new(),
        })
    }

    pub fn register(&self, info: SubagentInfo, cancel: tokio_util::sync::CancellationToken) {
        self.entries
            .insert(info.id.clone(), SubagentEntry { info, cancel });
    }

    /// 标记终态并取消其令牌（幂等）；返回是否存在该条目。
    pub fn finish(&self, id: &str, status: SubagentStatus) -> bool {
        if let Some(mut entry) = self.entries.get_mut(id) {
            entry.info.status = status;
            entry.cancel.cancel();
            true
        } else {
            false
        }
    }

    #[allow(dead_code)]
    fn cancel(&self, id: &str) -> bool {
        if let Some(entry) = self.entries.get(id) {
            entry.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// 条目的取消令牌（执行体监听它：逐项 finish / cancel_all 都经它传导）。
    pub fn cancel_token_of(&self, id: &str) -> Option<tokio_util::sync::CancellationToken> {
        self.entries.get(id).map(|e| e.cancel.clone())
    }

    /// 取消全部运行中子任务（主 agent 关停/排空时调用）。
    pub fn cancel_all(&self) -> usize {
        let ids: Vec<String> = self
            .entries
            .iter()
            .filter(|e| e.info.status == SubagentStatus::Running)
            .map(|e| e.info.id.clone())
            .collect();
        let count = ids.len();
        for id in ids {
            self.finish(&id, SubagentStatus::Cancelled);
        }
        count
    }

    /// 当前快照（含已完成条目，按启动时间排序）。
    pub fn snapshot(&self) -> Vec<SubagentInfo> {
        let mut list: Vec<SubagentInfo> = self.entries.iter().map(|e| e.info.clone()).collect();
        list.sort_by_key(|i| i.started_at_ms);
        list
    }

    /// 后台清扫：移除超时保留的终态条目。`cancel` 取消即退出（agent 关停）。
    pub fn spawn_sweeper(self: &Arc<Self>, cancel: tokio_util::sync::CancellationToken) {
        let store = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(600));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let now = chrono::Utc::now().timestamp_millis();
                        store.entries.retain(|_, entry| {
                            entry.info.status == SubagentStatus::Running
                                || now - entry.info.started_at_ms < COMPLETED_TTL.as_millis() as i64
                        });
                    }
                    _ = cancel.cancelled() => break,
                }
            }
        });
    }
}

/// 等"任一条取消链"：注册表条目的令牌 **或** 主 turn 的取消信号。
///
/// 为什么不是 `child_token()`：那需要 `CancellationToken` 这个具体类型，而
/// 主 turn 的令牌到了定义层只剩 [`echo_defs::tool::Cancellation`] 能力。
/// 两条链各自独立、任一触发即停——语义与 child token 等价。
pub async fn wait_cancelled(
    own: &tokio_util::sync::CancellationToken,
    parent: Option<&dyn echo_defs::tool::Cancellation>,
) {
    match parent {
        Some(parent) => {
            tokio::select! {
                _ = own.cancelled() => {}
                _ = parent.when_cancelled() => {}
            }
        }
        None => own.cancelled().await,
    }
}

/// The `spawn_subagent` tool: delegate an isolated subtask to a child agent.
///
/// 异步语义：调用立即返回子任务 id；子任务在后台以隔离上下文执行，完成时
/// 经 `<subagent_event>` hook 作为**新的入站分支**通知主 agent（见
/// 宿主 `Agent::dispatch_subagent_hook`）——主 turn 不等待，也不会因
/// 子任务的中间过程消耗上下文。
pub struct SpawnSubagentTool {
    store: Arc<SubagentStore>,
    /// 未指定 `timeout_secs` 时的默认墙钟上限（秒；`0` = 不限制）。
    ///
    /// 放在实例上而不是写死常量，是为了让"默认多久"成为**用户可配置**项：
    /// 组合根用 `AgentConfig::subagent_timeout_secs` 注入。配置项语义
    /// （`None` = 用内置 600 / `Some(0)` = 不限制 / `Some(n)` = n 秒）与这里的
    /// 取值一致，只差一层默认回退。
    default_timeout_secs: u64,
    /// 拉起子任务的执行闭包（由 Agent 注入，携带 provider/skills/事件发射等
    /// 依赖；保持 `Fn` 而非直接持有 `Arc<Agent>`，避免循环引用）。
    spawn: Arc<dyn Fn(SpawnRequest) + Send + Sync>,
    /// 远程委派聚合登记（宿主注入；`None` = 无人消费，见
    /// [`RemoteDelegationSink`]）。
    remote_sink: Option<Arc<dyn RemoteDelegationSink>>,
}

/// 远程委派的聚合登记接缝（宿主注入）。
///
/// **定义在定义层**（`echo_defs::subagent`，2026-11 S12）：声明方是本插件
/// （受理委派的一方），实现方是 `echo-plugin-federation`（聚合表的所有者）。
/// 此前 trait 住在这里，于是联邦插件要 `impl echo_plugin_subagent::...` 才能
/// 接上——聚合功能因此把两个插件绑成了 Cargo 依赖。
///
/// 实现方（组合根）通常转调 `federation::aggregator::register_remote_subagent`。
/// 不注入时：远程委派照常发出、`<subagent_event>` 逐个回报，只是没有分组
/// 聚合摘要——这是**降级而非错误**（单节点部署根本不需要聚合）。
pub use echo_defs::subagent::RemoteDelegationSink;

/// 一次委派的全部参数（工具参数解析后交给执行闭包）。
pub struct SpawnRequest {
    pub task_id: String,
    pub session_id: String,
    pub task: String,
    /// 墙钟上限；`None` = 不限制（慢模型/长任务）。
    ///
    /// 不限制**不等于**没有守卫：执行体始终挂着「连续 30 分钟无进展」看门狗
    /// （[`SUBAGENT_IDLE_TIMEOUT_SECS`]），卡死的任务照样会被中止并回报。
    pub timeout: Option<std::time::Duration>,
    /// 主 turn 的取消信号：主 turn 取消时子任务一并取消。
    ///
    /// 用定义层的 [`echo_defs::tool::Cancellation`] 而不是具体令牌类型——
    /// 本插件不依赖宿主的取消实现，宿主把它的令牌包一层递进来即可。
    pub parent_cancel: Option<Arc<dyn echo_defs::tool::Cancellation>>,
    /// 主 turn 的分支 id（子任务事件归属渲染用）。
    pub parent_branch_id: String,
    /// 目标联邦节点（federation Phase 3；None = 本机）。
    pub node: Option<String>,
    /// 子代理使用的 API profile 名（供应商池；None = 继承本 agent 的模型）。
    ///
    /// fail-closed 语义：名字不存在 / 缺 key / 构建失败时子任务**立即失败**
    /// 并以 `<subagent_event>` 回报（附可用清单），绝不静默回退主模型。
    pub profile: Option<String>,
}

#[async_trait::async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        SPAWN_SUBAGENT_TOOL
    }

    fn description(&self) -> &str {
        Self::DESCRIPTION
    }

    fn parameters(&self) -> Value {
        Self::schema()
    }

    /// **无上下文**调用（面板调试 / 直接经注册表执行）：会话与分支用占位值，
    /// 且不传播取消——子任务完成 hook 回灌到 `local:subagent::direct`。
    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        self.spawn(
            &arguments,
            "local:subagent::direct",
            None,
            "subagent",
        )
        .map_err(ToolError::InvalidArguments)
    }

    /// **主路径**：会话 / 分支 / 取消信号全部来自调用上下文。
    ///
    /// 历史上这里是宿主的 `run_orchestration` 特判（按工具名分支）外加
    /// `turn.rs` 一条并行的异步通道——两条路各自重复了事件日志。现在它就是
    /// 一个普通工具，只是要上下文（[`Tool::execute_in`]）。
    async fn execute_in(
        &self,
        cx: &echo_defs::tool::ToolContext,
        arguments: Value,
    ) -> Result<ToolResult, ToolError> {
        self.spawn(
            &arguments,
            &cx.session_id,
            cx.cancel.clone(),
            &cx.branch_id,
        )
        .map(ToolResult::text)
        .map_err(ToolError::InvalidArguments)
    }
}

impl SpawnSubagentTool {
    const DESCRIPTION: &'static str = "把独立子任务委派给一个隔离上下文的子 agent 执行。子 agent 看不到当前对话，task 必须自含全部背景与目标。子任务在后台执行，完成后会作为新事件回报结论；不要在同一轮里重复委派同一任务。适用于探索性检索、批量分析、需要大量中间步骤但只需结论的任务。参数: task(必填, 子任务的完整自含描述), timeout_secs(可选, 默认 600；**传 0 = 不设墙钟上限**，给慢模型/长任务用——无论如何都还有「无进展 30 分钟」看门狗兜底), node(可选, 联邦远程节点名——填入后子任务在该节点执行，多节点并行委派时结果会自动聚合汇报), profile(可选, 指定子代理使用的模型供应商 profile——如 \"kimi\"；可用名单见 list_api_profiles 工具，模型选择规范见 model-profiles 技能；不填 = 继承当前模型)。";

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "子任务的完整自含描述（背景、目标、产出要求）"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "执行超时秒数（默认 600，取值 30~86400）。传 0 = 不设墙钟上限，适合慢模型或需要长时间检索的任务；此时仍有「连续 30 分钟无进展」的看门狗兜底"
                },
                "node": {
                    "type": "string",
                    "description": "联邦远程节点名（peer 配置名）。来源：工具列表中 <peer>__<tool> 代理工具的前缀名（如 A100-40g__bash → A100-40g），或工作区会话的 [remote:<peer>] 标注。填入后子任务在该节点执行；填错/离线会立即失败（fail-closed，不会静默回本机）"
                },
                "profile": {
                    "type": "string",
                    "description": "子代理使用的 API profile 名（供应商池中的名称，如 \"kimi\"）。用于为本次委派换用不同模型；名字不存在/缺 key 时子任务立即失败并回报可用清单（fail-closed，不会静默回退主模型）。不填 = 继承当前 agent 模型。可用名单见 list_api_profiles 工具"
                }
            },
            "required": ["task"]
        })
    }

    /// 构造工具（无远程聚合 sink：远程委派不参与父 turn 分组）。
    pub fn new(store: Arc<SubagentStore>, spawn: Arc<dyn Fn(SpawnRequest) + Send + Sync>) -> Self {
        Self {
            store,
            spawn,
            remote_sink: None,
            default_timeout_secs: DEFAULT_SUBAGENT_TIMEOUT_SECS,
        }
    }

    /// 覆盖默认墙钟上限（秒；`0` = 不限制）。
    ///
    /// 组合根在装配期按 `AgentConfig::subagent_timeout_secs` 调用一次。
    /// 实例字段而非配置快照：解析在**受理时**发生，所以改配置后重启即生效。
    pub fn with_default_timeout_secs(mut self, secs: u64) -> Self {
        self.default_timeout_secs = secs;
        self
    }

    /// 注入远程委派聚合 sink（组合根装配期调用一次）。
    pub fn with_remote_sink(mut self, sink: Arc<dyn RemoteDelegationSink>) -> Self {
        self.remote_sink = Some(sink);
        self
    }

    /// 可选 sink（宿主装配点常用：`None` = 单机部署，不需要聚合摘要）。
    pub fn with_opt_remote_sink(mut self, sink: Option<Arc<dyn RemoteDelegationSink>>) -> Self {
        self.remote_sink = sink;
        self
    }

    /// 解析并校验参数；`session_id`/`parent_cancel`/`parent_branch_id` 由
    /// 执行环境注入（模型不可见）。返回立即可见的回执文本。
    pub fn spawn(
        &self,
        arguments: &Value,
        session_id: &str,
        parent_cancel: Option<Arc<dyn echo_defs::tool::Cancellation>>,
        parent_branch_id: &str,
    ) -> Result<String, String> {
        let task = arguments
            .get("task")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "spawn_subagent: `task`（非空字符串）必填——子 agent 看不到当前对话，任务描述必须自含背景与目标".to_string()
            })?
            .to_string();
        // 墙钟上限：未指定 → 用配置注入的默认；显式 0 → 不设上限（None）。
        // 显式正值按 [30s, 24h] 夹紧——上限放宽到一天是为了让"确实要跑很久"
        // 的任务有合法表达，同时仍然挡掉手滑写出的天文数字。
        let requested_secs = arguments
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.default_timeout_secs);
        let timeout = (requested_secs > 0).then(|| {
            std::time::Duration::from_secs(
                requested_secs.clamp(MIN_SUBAGENT_TIMEOUT_SECS, MAX_SUBAGENT_TIMEOUT_SECS),
            )
        });
        let task_id = uuid::Uuid::new_v4().to_string();
        // 注册表条目的令牌**独立**（`cancel_all` / 逐项 `finish` 只取消它）：
        // 父取消不再"生一个 child_token"（那需要具体令牌类型），而是在执行体
        // 里**同时等两条取消链**（见 `wait_cancelled`）。
        self.store.register(
            SubagentInfo {
                id: task_id.clone(),
                task: task.clone(),
                session_id: session_id.to_string(),
                started_at_ms: chrono::Utc::now().timestamp_millis(),
                status: SubagentStatus::Running,
            },
            tokio_util::sync::CancellationToken::new(),
        );
        let node = arguments
            .get("node")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // 子代理模型覆盖（供应商池 profile 名；执行体侧 fail-closed 解析）。
        let profile = arguments
            .get("profile")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        (self.spawn)(SpawnRequest {
            task_id: task_id.clone(),
            session_id: session_id.to_string(),
            task: task.clone(),
            timeout,
            parent_cancel,
            parent_branch_id: parent_branch_id.to_string(),
            node: node.clone(),
            profile: profile.clone(),
        });
        // P3-3 聚合登记：远程委派进父 turn 分组（全部终态后宿主产出聚合
        // 摘要投递父会话）。经 sink 交给宿主——本插件不认识联邦实现。
        if let (Some(ref peer), Some(sink)) = (&node, &self.remote_sink) {
            sink.register_remote_delegation(session_id, parent_branch_id, &task_id, peer, &task);
        }
        let target = node
            .as_deref()
            .map(|n| format!("（远程节点 {n}）"))
            .unwrap_or_default();
        let model_note = profile
            .as_deref()
            .map(|p| format!("（模型 profile: {p}）"))
            .unwrap_or_default();
        Ok(format!(
            "子任务已受理（id: {task_id}）{target}{model_note}，正在后台以隔离上下文执行。完成后会以 <subagent_event> 事件回报结论；你可以继续当前回复或处理其他事项。任务摘要：{}",
            truncate(&task, 120),
        ))
    }
}

/// 子 agent 循环：隔离上下文的一次性 turn（独立于内置循环与 echo-loop
/// 分派，避免递归 hook 注入）。返回最终回复文本。
///
/// 与主循环的差异：
/// - 系统提示词 = base + subagent 边界说明 + 任务；历史为空；
/// - 工具 = 注册表定义**剥离 `spawn_subagent`**（单层委派，深度上限
///   [`MAX_DELEGATION_DEPTH`]）；
/// - 不发 AgentThinking/LlmRequest 等会话级事件（子任务的观测走
///   `SubagentStarted/SubagentCompleted`），只记录推理回调给主分支。
#[allow(clippy::too_many_arguments)]
pub async fn run_subagent_turn(
    provider: Arc<dyn LlmProvider>,
    tools: Vec<echo_defs::tool::ToolDefinition>,
    registry: Arc<ToolRegistry>,
    base_prompt: String,
    task: String,
    cancel: tokio_util::sync::CancellationToken,
    max_iterations: usize,
    max_tokens: Option<u32>,
    // federation Phase 3：远程目标节点（peer 名）；Some 时工具调用名
    // 在执行前还原为注册名 `<peer>__<tool>`（见 `remote_tool_name`）。
    remote_peer: Option<&str>,
    // 推进回调：每轮 LLM 返回、每次工具调用完成各调一次。执行体的「无进展
    // 看门狗」据此区分**慢**与**卡死**（见 `SUBAGENT_IDLE_TIMEOUT_SECS`）。
    // 不传 = 没有看门狗（测试 / 直调用途）。
    progress: Option<ProgressFn>,
) -> Result<String> {
    let system_prompt = format!(
        "{base}\n\n# Subagent boundary\nYou are a subagent executing one delegated task in an isolated context. You cannot see the parent conversation and cannot message users directly. Work the task with the available tools, then answer with the final, self-contained result for the parent agent. Do not ask follow-up questions (nobody can answer); make reasonable assumptions and state them in the result.\n\n# Delegated task\n{task}",
        base = base_prompt,
        task = task,
    );
    let mut messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(task.clone()),
    ];
    let mut truncation_continues = 0usize;
    for _ in 0..max_iterations.max(1) {
        if cancel.is_cancelled() {
            return Err(anyhow::anyhow!(TURN_CANCELLED));
        }
        let model = provider.default_model().to_string();
        let request = ChatRequest {
            model,
            messages: messages.clone(),
            tools: Some(tools.clone()),
            temperature: None,
            max_tokens,
        };
        let response = tokio::select! {
            response = provider.chat(&request) => response?,
            _ = cancel.cancelled() => return Err(anyhow::anyhow!(TURN_CANCELLED)),
        };
        // 一次 LLM 往返 = 一次推进（哪怕它只产出工具调用或空内容）。
        if let Some(tick) = progress.as_ref() {
            tick();
        }
        if response.truncated() {
            truncation_continues += 1;
            if truncation_continues > MAX_TRUNCATION_CONTINUES {
                return Err(anyhow::anyhow!(
                    "subagent output was truncated at the token limit {} times; giving up",
                    MAX_TRUNCATION_CONTINUES
                ));
            }
            if let Some(text) = &response.content {
                if !text.trim().is_empty() {
                    messages.push(ChatMessage::assistant_with_reasoning(
                        text.clone(),
                        response.reasoning_content.clone(),
                    ));
                }
            }
            messages.push(ChatMessage::user(TRUNCATION_CONTINUE_PROMPT));
            continue;
        }
        if response.tool_calls.is_empty() {
            return Ok(response.content.unwrap_or_default());
        }
        messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: response.content.clone().unwrap_or_default(),
            reasoning_content: response.reasoning_content.clone(),
            tool_calls: Some(response.tool_calls.clone()),
            tool_call_id: None,
            images: vec![],
        });
        for call in &response.tool_calls {
            if cancel.is_cancelled() {
                return Err(anyhow::anyhow!(TURN_CANCELLED));
            }
            let args: Value =
                serde_json::from_str(&call.arguments).unwrap_or(Value::Object(Default::default()));
            // federation Phase 3：远程子任务（remote_peer 指定）的工具名
            // 还原为注册名 `<peer>__<tool>`——模型看到的是剥前缀的远程
            // 语境名称（2026-10 起分隔符为双下划线，与注册端同源）。
            let tool_name = match remote_peer {
                Some(peer) => {
                    let prefix = remote_tool_prefix(peer);
                    if call.name.starts_with(&prefix) {
                        call.name.clone()
                    } else {
                        remote_tool_name(peer, &call.name)
                    }
                }
                None => call.name.clone(),
            };
            let result = registry.execute_rich(&tool_name, args).await;
            let text = match result {
                Ok(r) => echo_defs::media::compact_embedded_media(&r.text, &r.images),
                Err(e) => format!("error: {e}"),
            };
            messages.push(ChatMessage::tool(text, &call.id));
            // 一次工具往返 = 一次推进（长跑工具期间看门狗不会误判：它在
            // 完工后打点，而工具自身上限 300s 远小于 30 分钟的空闲阈值）。
            if let Some(tick) = progress.as_ref() {
                tick();
            }
        }
    }
    Err(anyhow::anyhow!(
        "subagent reached max tool iterations ({}) without a final reply",
        max_iterations
    ))
}

/// 截断子任务结论（超限标注，主上下文预算保护）。
pub fn truncate_result(text: &str) -> String {
    if text.chars().count() <= MAX_SUBAGENT_RESULT_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(MAX_SUBAGENT_RESULT_CHARS).collect();
    format!(
        "{kept}\n…[结果过长已截断，共 {} 字符]",
        text.chars().count()
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn spawn_parses_optional_node() {
        let store = SubagentStore::new();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured2 = captured.clone();
        let tool = SpawnSubagentTool::new(
            store,
            std::sync::Arc::new(move |req: SpawnRequest| {
                *captured2.lock().unwrap() = Some(req.node);
            }),
        );
        // 无 node：Some(None)（被调用过、node 为 None）
        tool.spawn(
            &serde_json::json!({"task": "t"}),
            "s",
            None,
            "b",
        )
        .unwrap();
        assert_eq!(captured.lock().unwrap().clone(), Some(None));
        // 有 node：透传
        tool.spawn(
            &serde_json::json!({"task": "t", "node": "gpu-box"}),
            "s",
            None,
            "b",
        )
        .unwrap();
        assert_eq!(
            captured.lock().unwrap().clone().flatten(),
            Some("gpu-box".to_string())
        );
    }

    #[test]
    fn spawn_parses_optional_profile() {
        let store = SubagentStore::new();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured2 = captured.clone();
        let tool = SpawnSubagentTool::new(
            store,
            std::sync::Arc::new(move |req: SpawnRequest| {
                *captured2.lock().unwrap() = Some(req.profile);
            }),
        );
        // 无 profile：Some(None)（被调用过、profile 为 None）
        tool.spawn(
            &serde_json::json!({"task": "t"}),
            "s",
            None,
            "b",
        )
        .unwrap();
        assert_eq!(captured.lock().unwrap().clone(), Some(None));
        // 有 profile：trim 后透传
        tool.spawn(
            &serde_json::json!({"task": "t", "profile": "  kimi "}),
            "s",
            None,
            "b",
        )
        .unwrap();
        assert_eq!(
            captured.lock().unwrap().clone().flatten(),
            Some("kimi".to_string())
        );
        // 空串 = 未指定
        tool.spawn(
            &serde_json::json!({"task": "t", "profile": "   "}),
            "s",
            None,
            "b",
        )
        .unwrap();
        assert_eq!(captured.lock().unwrap().clone().flatten(), None);
    }

    use super::*;

    fn tool(
        spawn: Arc<dyn Fn(SpawnRequest) + Send + Sync>,
    ) -> (SpawnSubagentTool, Arc<SubagentStore>) {
        let store = SubagentStore::new();
        (SpawnSubagentTool::new(store.clone(), spawn), store)
    }

    #[test]
    fn spawn_registers_and_returns_receipt() {
        let spawned = Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawned2 = spawned.clone();
        let (tool, store) = tool(Arc::new(move |req| {
            spawned2.lock().unwrap().push(req.task_id)
        }));
        let receipt = tool
            .spawn(
                &json!({"task": "找出所有 X 的用法"}),
                "local:tui::one",
                None,
                "branch-1",
            )
            .expect("spawn ok");
        assert!(receipt.contains("子任务已受理"), "{receipt}");
        let list = store.snapshot();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, SubagentStatus::Running);
        assert_eq!(list[0].task, "找出所有 X 的用法");
        assert_eq!(spawned.lock().unwrap().len(), 1);
    }

    #[test]
    fn spawn_rejects_empty_task_with_corrective_message() {
        let (tool, store) = tool(Arc::new(|_| {}));
        let error = tool
            .spawn(
                &json!({"task": "  "}),
                "local:tui::one",
                None,
                "b",
            )
            .unwrap_err();
        assert!(error.contains("task"), "{error}");
        assert!(error.contains("自含"), "{error}");
        assert!(
            store.snapshot().is_empty(),
            "rejected spawn registers nothing"
        );
    }

    /// 取消传播：注册表条目的令牌与主 turn 的取消信号**任一触发都停**。
    ///
    /// 语义来源：`SpawnRequest::parent_cancel` 是定义层的 `Cancellation` 能力
    /// （不是具体令牌类型），所以执行体同时等两条链——等价于过去的
    /// `child_token()`，但不把运行时类型带进定义层。
    #[tokio::test]
    async fn either_cancel_chain_stops_the_task() {
        /// 包一层 `CancellationToken`：定义层只认能力。
        struct Token(tokio_util::sync::CancellationToken);
        impl echo_defs::tool::Cancellation for Token {
            fn is_cancelled(&self) -> bool {
                self.0.is_cancelled()
            }
            fn when_cancelled(
                &self,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
                Box::pin(self.0.cancelled())
            }
        }

        // ① 注册表条目令牌取消 → 停
        let own = tokio_util::sync::CancellationToken::new();
        let handle = tokio::spawn({
            let own = own.clone();
            async move { wait_cancelled(&own, None).await }
        });
        own.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("条目令牌取消后应立刻返回")
            .unwrap();

        // ② 主 turn 取消 → 停（条目令牌没动）
        let own = tokio_util::sync::CancellationToken::new();
        let parent = Arc::new(Token(tokio_util::sync::CancellationToken::new()));
        let handle = tokio::spawn({
            let own = own.clone();
            let parent = parent.clone();
            async move { wait_cancelled(&own, Some(parent.as_ref() as &dyn echo_defs::tool::Cancellation)).await }
        });
        (parent.0).cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("父取消后应立刻返回")
            .unwrap();
        assert!(!own.is_cancelled(), "条目自己的令牌不该被连带取消");
    }

    #[test]
    fn timeout_is_clamped_and_defaulted() {
        let timeouts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let timeouts2 = timeouts.clone();
        let (tool, _) = tool(Arc::new(move |req| {
            timeouts2.lock().unwrap().push(req.timeout)
        }));
        tool.spawn(
            &json!({"task": "a"}),
            "s",
            None,
            "b",
        )
        .unwrap();
        tool.spawn(
            &json!({"task": "b", "timeout_secs": 1}),
            "s",
            None,
            "b",
        )
        .unwrap();
        let got = timeouts.lock().unwrap().clone();
        assert_eq!(got[0], Some(std::time::Duration::from_secs(600)), "default");
        assert_eq!(got[1], Some(std::time::Duration::from_secs(30)), "clamped to min");
    }

    /// 2026-10 用户诉求：「kimi 太慢，能不能不设时间限制」。
    ///
    /// 三种取值各有明确语义，别混：未指定 → 配置默认；`0` → 不设墙钟上限；
    /// 正值 → 夹到 [30s, 24h]。上限从旧的 1 小时放宽到 24 小时同样是这次改动
    /// 的一部分——「确实要跑很久」需要有合法表达。
    #[test]
    fn timeout_zero_means_unlimited_and_positive_is_capped() {
        let timeouts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let timeouts2 = timeouts.clone();
        let (tool, _) = tool(Arc::new(move |req| {
            timeouts2.lock().unwrap().push(req.timeout)
        }));
        for args in [
            json!({"task": "unlimited", "timeout_secs": 0}),
            json!({"task": "huge", "timeout_secs": 999_999}),
            json!({"task": "half-hour", "timeout_secs": 1800}),
        ] {
            tool.spawn(&args, "s", None, "b").unwrap();
        }
        let got = timeouts.lock().unwrap().clone();
        assert_eq!(got[0], None, "0 = 不设上限");
        assert_eq!(got[1], Some(std::time::Duration::from_secs(86_400)), "夹到 24h");
        assert_eq!(got[2], Some(std::time::Duration::from_secs(1800)), "正常值原样透传");
    }

    /// 配置注入的默认值必须真的生效（而不是被常量吃掉）。
    #[test]
    fn default_timeout_secs_comes_from_config_injection() {
        let timeouts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let timeouts2 = timeouts.clone();
        let (tool, _) = tool(Arc::new(move |req| {
            timeouts2.lock().unwrap().push(req.timeout)
        }));
        // 宿主注入 0（配置 subagent_timeout_secs = 0 → 全部不限制）
        let tool = tool.with_default_timeout_secs(0);
        tool.spawn(&json!({"task": "a"}), "s", None, "b").unwrap();
        // 显式值仍然压过默认
        tool.spawn(&json!({"task": "b", "timeout_secs": 120}), "s", None, "b")
            .unwrap();
        let got = timeouts.lock().unwrap().clone();
        assert_eq!(got[0], None, "注入 0 后未指定 = 不限制");
        assert_eq!(got[1], Some(std::time::Duration::from_secs(120)));
    }

    #[test]
    fn result_truncation_bounds_context_cost() {
        let short = "结论：一切正常";
        assert_eq!(truncate_result(short), short);
        let long = "x".repeat(MAX_SUBAGENT_RESULT_CHARS * 2);
        let truncated = truncate_result(&long);
        assert!(truncated.contains("已截断"), "{truncated}");
        assert!(truncated.chars().count() < MAX_SUBAGENT_RESULT_CHARS + 100);
    }

    #[test]
    fn cancel_all_only_cancels_running() {
        let store = SubagentStore::new();
        for (id, status) in [
            ("a", SubagentStatus::Running),
            ("b", SubagentStatus::Running),
        ] {
            store.register(
                SubagentInfo {
                    id: id.into(),
                    task: "t".into(),
                    session_id: "s".into(),
                    started_at_ms: 0,
                    status,
                },
                tokio_util::sync::CancellationToken::new(),
            );
        }
        assert_eq!(store.cancel_all(), 2);
        assert!(store
            .snapshot()
            .iter()
            .all(|i| i.status == SubagentStatus::Cancelled));
        assert_eq!(store.cancel_all(), 0, "idempotent");
    }

    /// 一次委派登记的快照：(session, branch, call_id, peer, task)。
    type RecordedDelegation = (String, String, String, String, String);

    /// 记录远程委派登记的假 sink（替换宿主的联邦聚合器）。
    #[derive(Default)]
    struct RecordingSink {
        calls: std::sync::Mutex<Vec<RecordedDelegation>>,
    }

    impl RecordingSink {
        fn calls(&self) -> Vec<RecordedDelegation> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl RemoteDelegationSink for RecordingSink {
        fn register_remote_delegation(
            &self,
            session_id: &str,
            parent_branch_id: &str,
            call_id: &str,
            peer: &str,
            task: &str,
        ) {
            self.calls.lock().unwrap().push((
                session_id.to_string(),
                parent_branch_id.to_string(),
                call_id.to_string(),
                peer.to_string(),
                task.to_string(),
            ));
        }
    }

    #[test]
    fn remote_delegation_goes_through_injected_sink() {
        let sink = Arc::new(RecordingSink::default());
        let store = SubagentStore::new();
        let tool =
            SpawnSubagentTool::new(store.clone(), Arc::new(|_| {})).with_remote_sink(sink.clone());
        tool.spawn(
            &json!({"task": "在 A 上检索日志", "node": "gpu-a"}),
            "sess-1",
            None,
            "turn-7",
        )
        .unwrap();
        let calls = sink.calls();
        assert_eq!(calls.len(), 1);
        let (session, branch, call_id, peer, task) = &calls[0];
        // 聚合分组的三个键必须来自本次受理，而不是库里存的默认值
        assert_eq!(session, "sess-1");
        assert_eq!(branch, "turn-7");
        assert_eq!(peer, "gpu-a");
        assert_eq!(task, "在 A 上检索日志");
        // call_id 与注册表里的任务 id 一致（终态销账靠它配对）
        assert_eq!(call_id, &store.snapshot()[0].id);
    }

    #[test]
    fn local_delegation_never_touches_the_sink() {
        let sink = Arc::new(RecordingSink::default());
        let tool = SpawnSubagentTool::new(SubagentStore::new(), Arc::new(|_| {}))
            .with_remote_sink(sink.clone());
        tool.spawn(
            &json!({"task": "本机任务"}),
            "sess-1",
            None,
            "turn-7",
        )
        .unwrap();
        assert!(sink.calls().is_empty(), "本机委派不进远程聚合分组");
    }

    #[test]
    fn absent_sink_degrades_instead_of_failing() {
        // 单节点部署 / 未装配联邦：远程委派照常受理，只是没有聚合摘要。
        let store = SubagentStore::new();
        let tool = SpawnSubagentTool::new(store.clone(), Arc::new(|_| {}));
        let receipt = tool
            .spawn(
                &json!({"task": "远程任务", "node": "peer-x"}),
                "s",
                None,
                "b",
            )
            .expect("absence of a sink must not fail the spawn");
        assert!(receipt.contains("peer-x"), "{receipt}");
        assert_eq!(store.snapshot().len(), 1);
    }

    #[test]
    fn tool_contract_is_stable() {
        // 工具名与 schema 是模型契约：改名会让历史会话里的调用失效。
        let tool = SpawnSubagentTool::new(SubagentStore::new(), Arc::new(|_| {}));
        assert_eq!(tool.name(), "spawn_subagent");
        assert_eq!(SPAWN_SUBAGENT_TOOL, "spawn_subagent");
        let schema = tool.parameters();
        assert_eq!(schema["required"][0], "task");
        for key in ["task", "timeout_secs", "node", "profile"] {
            assert!(schema["properties"].get(key).is_some(), "{key}");
        }
    }

    #[test]
    fn hook_envelope_is_structured() {
        let payload = json!({"event": "subagent_event", "subagent_id": "x", "success": true});
        let hook = wrap_subagent_event(&payload);
        assert!(hook.contains("<subagent_event>"));
        assert!(hook.contains("</subagent_event>"));
        assert!(hook.contains("\"success\": true"));
    }
}

/// 完成 / 拒绝钩子中的 `task` 字段（2026-10 观测缺口修复）。
///
/// 此前固定截断到 500 字符（审计实测 53/58 条 task 恰 501 字符，任务描述
/// 无法完整审计）。现放宽到 4000 字符并在超限时显式标注；完整参数（含 task
/// 全文）随 `spawn_subagent` 的 ToolCall 事件落库。
pub fn subagent_task_field(task: &str) -> String {
    const MAX_TASK_CHARS: usize = 4000;
    if task.chars().count() <= MAX_TASK_CHARS {
        return task.to_string();
    }
    let kept: String = task.chars().take(MAX_TASK_CHARS).collect();
    format!(
        "{kept}\n…[task 过长已截断，共 {} 字符；完整文本见该 spawn_subagent 调用的 arguments]",
        task.chars().count()
    )
}

/// 本次失败是否属于"随主任务取消"（而不是真失败）。
///
/// 判据是 `anyhow` 错误的**显示文本**等于定义层的 `TURN_CANCELLED`——单点
/// 定义在 `echo_defs::llm`，三方（循环、子代理、宿主）共用同一口径，避免
/// "取消被当成失败"报警到面板。
pub fn is_turn_cancelled(error: &anyhow::Error) -> bool {
    error.to_string() == echo_defs::llm::TURN_CANCELLED
}
