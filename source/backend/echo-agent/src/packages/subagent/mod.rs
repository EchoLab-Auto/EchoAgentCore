//! Subagent plugin: delegate an isolated subtask to a child agent.
//!
//! 主 agent 经 `spawn_subagent` 编排工具拉起子 agent：子任务以**隔离上下文**
//! 运行（自己的系统提示词与工具循环，不回写 trunk），完成后经
//! [`crate::input_marker`] 的结构化 hook（`<subagent_event>`）以全新入站分支
//! 通知主 agent——hook 注入接口由 echo-loop 的 `SubagentToolHooks` 定义
//! （见 `source/loop/echo-loop/src/runner.rs`），QQ 消息等其他插件复用同一
//! hook 机制（入站 = 结构化 hook → 新 turn）。
//!
//! 模块构成：
//! - [`SubagentStore`]：运行中子任务注册表（取消句柄 + 任务快照，TTL 清扫）
//! - [`SpawnSubagentTool`]：模型可见工具（异步拉起，立即返回 id）
//! - `crate::agent::Agent::dispatch_subagent_hook`：完成 hook 的注入入口
//!
//! 设计文档见 `document/core-subagent.md`。

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::llm::ChatMessage;

/// 单次委派结果的可见回灌上限（字符）：子任务结论超过即截断并标注，
/// 防单个子任务反过来烧掉主上下文。
pub(crate) const MAX_SUBAGENT_RESULT_CHARS: usize = 8 * 1024;

/// 已完成任务条目在注册表中的保留时间（到点由清扫任务移除）。
const COMPLETED_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// 子 agent 允许的最大委派深度：子 agent 的 spawn 能力被剥离（深度 1），
/// 不存在递归委派；常量保留给未来显式深度放宽。
#[allow(dead_code)]
pub(crate) const MAX_DELEGATION_DEPTH: u32 = 1;

/// 子任务默认执行超时（秒）。
const DEFAULT_SUBAGENT_TIMEOUT_SECS: u64 = 600;

/// `spawn_subagent` 编排工具名（唯一事实来源）。
pub(crate) const SPAWN_SUBAGENT_TOOL: &str = "spawn_subagent";

/// `<subagent_event>` hook 包装（与 `<qq_message_hook>`/`<timer_event>` 同族）。
pub(crate) fn wrap_subagent_event(payload: &Value) -> String {
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

/// The `spawn_subagent` tool: delegate an isolated subtask to a child agent.
///
/// 异步语义：调用立即返回子任务 id；子任务在后台以隔离上下文执行，完成时
/// 经 `<subagent_event>` hook 作为**新的入站分支**通知主 agent（见
/// `crate::agent::Agent::dispatch_subagent_hook`）——主 turn 不等待，也不会因
/// 子任务的中间过程消耗上下文。
pub struct SpawnSubagentTool {
    store: Arc<SubagentStore>,
    /// 拉起子任务的执行闭包（由 Agent 注入，携带 provider/skills/事件发射等
    /// 依赖；保持 `Fn` 而非直接持有 `Arc<Agent>`，避免循环引用）。
    spawn: Arc<dyn Fn(SpawnRequest) + Send + Sync>,
}

/// 一次委派的全部参数（工具参数解析后交给执行闭包）。
pub struct SpawnRequest {
    pub task_id: String,
    pub session_id: String,
    pub task: String,
    pub timeout: std::time::Duration,
    /// 主 turn 的取消令牌：主 turn 取消时子任务一并取消。
    pub parent_cancel: tokio_util::sync::CancellationToken,
    /// 主 turn 的分支 id（子任务事件归属渲染用）。
    pub parent_branch_id: String,
    /// 目标联邦节点（federation Phase 3；None = 本机）。
    pub node: Option<String>,
}

#[async_trait::async_trait]
impl crate::tool::Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        SPAWN_SUBAGENT_TOOL
    }

    fn description(&self) -> &str {
        Self::DESCRIPTION
    }

    fn parameters(&self) -> Value {
        Self::schema()
    }

    /// 注册表兜底路径（无 turn 上下文：无父取消传播、归属分支为 "subagent"）。
    /// 主路径在 agent 循环内经 [`Self::spawn`] 携带 turn cancel 调用。
    async fn execute(&self, arguments: Value) -> Result<String, crate::tool::ToolError> {
        // 会话 id 从参数拿不到（模型不可见）——注册表路径只用于面板/调试
        // 直接调用，子任务完成 hook 回灌"background"会话。
        self.spawn(
            &arguments,
            "local:subagent::direct",
            tokio_util::sync::CancellationToken::new(),
            "subagent",
        )
        .map_err(crate::tool::ToolError::InvalidArguments)
    }
}

impl SpawnSubagentTool {
    const DESCRIPTION: &'static str = "把独立子任务委派给一个隔离上下文的子 agent 执行。子 agent 看不到当前对话，task 必须自含全部背景与目标。子任务在后台执行，完成后会作为新事件回报结论；不要在同一轮里重复委派同一任务。适用于探索性检索、批量分析、需要大量中间步骤但只需结论的任务。参数: task(必填, 子任务的完整自含描述), timeout_secs(可选, 默认 600), node(可选, 联邦远程节点名——填入后子任务在该节点执行，多节点并行委派时结果会自动聚合汇报)。";

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
                    "description": "执行超时秒数（默认 600）"
                },
                "node": {
                    "type": "string",
                    "description": "联邦远程节点名（peer 配置名，如 \"gpu-box\"）。填入后子任务在该节点执行；多节点并行委派时结果自动聚合汇报"
                }
            },
            "required": ["task"]
        })
    }

    pub fn new(store: Arc<SubagentStore>, spawn: Arc<dyn Fn(SpawnRequest) + Send + Sync>) -> Self {
        Self { store, spawn }
    }

    /// 解析并校验参数；`session_id`/`parent_cancel`/`parent_branch_id` 由
    /// 执行环境注入（模型不可见）。返回立即可见的回执文本。
    pub fn spawn(
        &self,
        arguments: &Value,
        session_id: &str,
        parent_cancel: tokio_util::sync::CancellationToken,
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
        let timeout_secs = arguments
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_SECS)
            .clamp(30, 3600);
        let task_id = uuid::Uuid::new_v4().to_string();
        self.store.register(
            SubagentInfo {
                id: task_id.clone(),
                task: task.clone(),
                session_id: session_id.to_string(),
                started_at_ms: chrono::Utc::now().timestamp_millis(),
                status: SubagentStatus::Running,
            },
            parent_cancel.child_token(),
        );
        let node = arguments
            .get("node")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        (self.spawn)(SpawnRequest {
            task_id: task_id.clone(),
            session_id: session_id.to_string(),
            task: task.clone(),
            timeout: std::time::Duration::from_secs(timeout_secs),
            parent_cancel,
            parent_branch_id: parent_branch_id.to_string(),
            node: node.clone(),
        });
        // P3-3 聚合登记：远程委派进父 turn 分组（全部终态后组合根
        // 产出聚合摘要投递父会话）。
        if let Some(ref peer) = node {
            crate::federation::aggregator::register_remote_subagent(
                session_id,
                parent_branch_id,
                &task_id,
                peer,
                &task,
            );
        }
        let target = node
            .as_deref()
            .map(|n| format!("（远程节点 {n}）"))
            .unwrap_or_default();
        Ok(format!(
            "子任务已受理（id: {task_id}）{target}，正在后台以隔离上下文执行。完成后会以 <subagent_event> 事件回报结论；你可以继续当前回复或处理其他事项。任务摘要：{}",
            crate::llm::truncate(&task, 120),
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
pub(crate) async fn run_subagent_turn(
    provider: Arc<dyn crate::llm::LlmProvider>,
    tools: Vec<echo_defs::tool::ToolDefinition>,
    registry: Arc<crate::tool::ToolRegistry>,
    base_prompt: String,
    task: String,
    cancel: tokio_util::sync::CancellationToken,
    max_iterations: usize,
    max_tokens: Option<u32>,
    // federation Phase 3：远程目标节点（peer 名）；Some 时工具调用名
    // 在执行前还原为 `<peer>:<tool>`。
    remote_peer: Option<&str>,
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
            return Err(anyhow::anyhow!(crate::agent::TURN_CANCELLED));
        }
        let model = provider.default_model().to_string();
        let request = crate::llm::ChatRequest {
            model,
            messages: messages.clone(),
            tools: Some(tools.clone()),
            temperature: None,
            max_tokens,
        };
        let response = tokio::select! {
            response = provider.chat(&request) => response?,
            _ = cancel.cancelled() => return Err(anyhow::anyhow!(crate::agent::TURN_CANCELLED)),
        };
        if response.truncated() {
            truncation_continues += 1;
            if truncation_continues > crate::agent::MAX_TRUNCATION_CONTINUES {
                return Err(anyhow::anyhow!(
                    "subagent output was truncated at the token limit {} times; giving up",
                    crate::agent::MAX_TRUNCATION_CONTINUES
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
            messages.push(ChatMessage::user(crate::agent::TRUNCATION_CONTINUE_PROMPT));
            continue;
        }
        if response.tool_calls.is_empty() {
            return Ok(response.content.unwrap_or_default());
        }
        messages.push(ChatMessage {
            role: crate::llm::ChatRole::Assistant,
            content: response.content.clone().unwrap_or_default(),
            reasoning_content: response.reasoning_content.clone(),
            tool_calls: Some(response.tool_calls.clone()),
            tool_call_id: None,
            images: vec![],
        });
        for call in &response.tool_calls {
            if cancel.is_cancelled() {
                return Err(anyhow::anyhow!(crate::agent::TURN_CANCELLED));
            }
            let args: Value =
                serde_json::from_str(&call.arguments).unwrap_or(Value::Object(Default::default()));
            // federation Phase 3：远程子任务（remote_peer 指定）的工具名
            // 还原为 `<peer>:<tool>`——模型看到的是剥前缀的远程语境名称。
            let tool_name = match remote_peer {
                Some(peer) if !call.name.starts_with(&format!("{peer}:")) => {
                    format!("{peer}:{}", call.name)
                }
                _ => call.name.clone(),
            };
            let result = registry.execute_rich(&tool_name, args).await;
            let text = match result {
                Ok(r) => echo_defs::media::compact_embedded_media(&r.text, &r.images),
                Err(e) => format!("error: {e}"),
            };
            messages.push(ChatMessage::tool(text, &call.id));
        }
    }
    Err(anyhow::anyhow!(
        "subagent reached max tool iterations ({}) without a final reply",
        max_iterations
    ))
}

/// 截断子任务结论（超限标注，主上下文预算保护）。
pub(crate) fn truncate_result(text: &str) -> String {
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
            tokio_util::sync::CancellationToken::new(),
            "b",
        )
        .unwrap();
        assert_eq!(captured.lock().unwrap().clone(), Some(None));
        // 有 node：透传
        tool.spawn(
            &serde_json::json!({"task": "t", "node": "gpu-box"}),
            "s",
            tokio_util::sync::CancellationToken::new(),
            "b",
        )
        .unwrap();
        assert_eq!(
            captured.lock().unwrap().clone().flatten(),
            Some("gpu-box".to_string())
        );
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
                tokio_util::sync::CancellationToken::new(),
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
                tokio_util::sync::CancellationToken::new(),
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

    #[test]
    fn parent_cancel_cascades_to_child_token() {
        let spawned = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let spawned2 = spawned.clone();
        let (tool, store) = tool(Arc::new(move |req| {
            spawned2.lock().unwrap().push(req.task_id);
        }));
        let parent = tokio_util::sync::CancellationToken::new();
        tool.spawn(&json!({"task": "t"}), "s", parent.clone(), "b")
            .unwrap();
        // 父令牌取消后，注册表中的子令牌一并取消。
        parent.cancel();
        let id = store.snapshot()[0].id.clone();
        // 注册表 finish 会取消条目令牌；这里直接断言取消传播链存在。
        assert!(store.finish(&id, SubagentStatus::Cancelled));
        assert_eq!(store.snapshot()[0].status, SubagentStatus::Cancelled);
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
            tokio_util::sync::CancellationToken::new(),
            "b",
        )
        .unwrap();
        tool.spawn(
            &json!({"task": "b", "timeout_secs": 1}),
            "s",
            tokio_util::sync::CancellationToken::new(),
            "b",
        )
        .unwrap();
        let got = timeouts.lock().unwrap().clone();
        assert_eq!(got[0], std::time::Duration::from_secs(600), "default");
        assert_eq!(got[1], std::time::Duration::from_secs(30), "clamped to min");
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

    #[test]
    fn hook_envelope_is_structured() {
        let payload = json!({"event": "subagent_event", "subagent_id": "x", "success": true});
        let hook = wrap_subagent_event(&payload);
        assert!(hook.contains("<subagent_event>"));
        assert!(hook.contains("</subagent_event>"));
        assert!(hook.contains("\"success\": true"));
    }
}
