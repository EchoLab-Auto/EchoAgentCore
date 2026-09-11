//! Agent-native orchestration tools.
//!
//! These tools need the current session or LLM provider, so they are owned by
//! [`Agent`](super::Agent) instead of the platform-independent `ToolRegistry`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::llm::{ChatMessage, ChatRequest, LlmProvider, ToolDefinition};
use crate::session::SessionKey;
use crate::tool::ToolRegistry;
use crate::SelfUpdateConfig;

const MAX_TIMER_DELAY: Duration = Duration::from_secs(31 * 24 * 60 * 60);
const MAX_TASK_CHARS: usize = 8_000;
const MAX_PARALLEL_BRANCHES: usize = 8;
const MAX_BACKGROUND_TOOL_ITERATIONS: usize = 64;
const MAX_BACKGROUND_TASK_HISTORY: usize = 128;
const UPDATE_SERVICE: &str = "echo-agent-core-update.service";

#[derive(Debug, Clone)]
pub(super) struct TimerEvent {
    pub id: String,
    pub session_id: String,
    pub task: String,
    pub due_at: DateTime<Utc>,
}

#[derive(Debug)]
struct TimerEntry {
    session_id: String,
    task: String,
    due_at: DateTime<Utc>,
    cancel: CancellationToken,
}

/// In-memory timer storage and delivery queue.
pub(super) struct TimerScheduler {
    timers: Arc<Mutex<HashMap<String, TimerEntry>>>,
    event_tx: mpsc::UnboundedSender<TimerEvent>,
    event_rx: Mutex<Option<mpsc::UnboundedReceiver<TimerEvent>>>,
    shutdown: CancellationToken,
}

impl TimerScheduler {
    pub fn new(shutdown: CancellationToken) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            timers: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
            event_rx: Mutex::new(Some(event_rx)),
            shutdown,
        }
    }

    pub fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<TimerEvent>> {
        self.event_rx.try_lock().ok()?.take()
    }

    pub async fn schedule(&self, session_id: &str, arguments: Value) -> Result<String, String> {
        let args: ScheduleTimerArgs = serde_json::from_value(arguments)
            .map_err(|error| format!("invalid timer arguments: {error}"))?;
        validate_task(&args.task)?;
        let due_at = resolve_due_at(&args)?;
        let delay = due_at
            .signed_duration_since(Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        if delay > MAX_TIMER_DELAY {
            return Err("timer cannot be scheduled more than 31 days ahead".into());
        }

        let id = uuid::Uuid::new_v4().to_string();
        let timer_cancel = CancellationToken::new();
        self.timers.lock().await.insert(
            id.clone(),
            TimerEntry {
                session_id: session_id.to_string(),
                task: args.task.clone(),
                due_at,
                cancel: timer_cancel.clone(),
            },
        );

        let event = TimerEvent {
            id: id.clone(),
            session_id: session_id.to_string(),
            task: args.task,
            due_at,
        };
        let event_tx = self.event_tx.clone();
        let timers = Arc::clone(&self.timers);
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {
                    if event_tx.send(event.clone()).is_err() {
                        timers.lock().await.remove(&event.id);
                    }
                }
                _ = timer_cancel.cancelled() => {}
                _ = shutdown.cancelled() => {}
            }
        });

        Ok(json!({
            "timer_id": id,
            "due_at": due_at.to_rfc3339_opts(SecondsFormat::Secs, true),
            "status": "scheduled"
        })
        .to_string())
    }

    pub async fn list(&self, session_id: &str) -> String {
        let timers = self.timers.lock().await;
        let mut rows: Vec<_> = timers
            .iter()
            .filter(|(_, timer)| timer.session_id == session_id)
            .map(|(id, timer)| {
                json!({
                    "timer_id": id,
                    "due_at": timer.due_at.to_rfc3339_opts(SecondsFormat::Secs, true),
                    "task": timer.task
                })
            })
            .collect();
        rows.sort_by(|left, right| left["due_at"].as_str().cmp(&right["due_at"].as_str()));
        json!({ "timers": rows }).to_string()
    }

    pub async fn cancel(&self, session_id: &str, arguments: Value) -> Result<String, String> {
        let args: CancelTimerArgs = serde_json::from_value(arguments)
            .map_err(|error| format!("invalid cancel arguments: {error}"))?;
        let mut timers = self.timers.lock().await;
        let belongs_to_session = timers
            .get(&args.timer_id)
            .is_some_and(|timer| timer.session_id == session_id);
        if !belongs_to_session {
            return Err("timer not found in the current session".into());
        }
        let timer = timers
            .remove(&args.timer_id)
            .expect("timer existence checked above");
        timer.cancel.cancel();
        Ok(json!({ "timer_id": args.timer_id, "status": "cancelled" }).to_string())
    }

    pub async fn mark_delivered(&self, timer_id: &str) {
        self.timers.lock().await.remove(timer_id);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum DeliveryTarget {
    Backend { session_id: String },
    QqPrivate { user_id: String },
    QqGroup { group_id: String },
}

impl DeliveryTarget {
    pub fn key(&self) -> String {
        match self {
            Self::Backend { session_id } => format!("backend:{session_id}"),
            Self::QqPrivate { user_id } => format!("qq_private:{user_id}"),
            Self::QqGroup { group_id } => format!("qq_group:{group_id}"),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct BackgroundBranch {
    pub branch_id: String,
    pub task: String,
    pub context: Option<String>,
    pub target: DeliveryTarget,
}

#[derive(Debug, Clone)]
pub(super) struct BackgroundWork {
    pub objective: String,
    pub branches: Vec<BackgroundBranch>,
}

#[derive(Debug, Clone)]
pub(super) struct BackgroundBranchResult {
    pub branch_id: String,
    pub target: DeliveryTarget,
    pub success: bool,
    pub result: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(super) struct BackgroundCompletion {
    pub sequence: u64,
    pub task_id: String,
    pub session_id: String,
    pub objective: String,
    pub created_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub branches: Vec<BackgroundBranchResult>,
    pub cancelled: bool,
}

pub(super) struct OrderedCompletionBuffer {
    next_sequence: u64,
    pending: std::collections::BTreeMap<u64, BackgroundCompletion>,
}

impl OrderedCompletionBuffer {
    pub fn new() -> Self {
        Self {
            next_sequence: 1,
            pending: std::collections::BTreeMap::new(),
        }
    }

    pub fn push(&mut self, completion: BackgroundCompletion) -> Vec<BackgroundCompletion> {
        self.pending.insert(completion.sequence, completion);
        let mut ready = Vec::new();
        while let Some(completion) = self.pending.remove(&self.next_sequence) {
            ready.push(completion);
            self.next_sequence += 1;
        }
        ready
    }
}

#[derive(Debug)]
struct BackgroundTaskEntry {
    session_id: String,
    objective: String,
    sequence: u64,
    created_at: DateTime<Utc>,
    state: &'static str,
    cancel: CancellationToken,
}

/// Runs isolated, tool-capable branches without holding the main conversation lock.
pub(super) struct BackgroundTaskManager {
    tasks: Arc<Mutex<HashMap<String, BackgroundTaskEntry>>>,
    event_tx: mpsc::UnboundedSender<BackgroundCompletion>,
    event_rx: Mutex<Option<mpsc::UnboundedReceiver<BackgroundCompletion>>>,
    next_sequence: AtomicU64,
    branch_slots: Arc<Semaphore>,
    shutdown: CancellationToken,
}

#[derive(Clone)]
pub(super) struct BackgroundRuntime {
    pub provider: Arc<dyn LlmProvider>,
    pub tools: Arc<ToolRegistry>,
    pub model: String,
    pub max_tool_iterations: usize,
    pub tool_timeout: Duration,
    pub history_snapshot: Vec<ChatMessage>,
}

impl BackgroundTaskManager {
    pub fn new(shutdown: CancellationToken) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
            event_rx: Mutex::new(Some(event_rx)),
            next_sequence: AtomicU64::new(1),
            branch_slots: Arc::new(Semaphore::new(MAX_PARALLEL_BRANCHES)),
            shutdown,
        }
    }

    pub fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<BackgroundCompletion>> {
        self.event_rx.try_lock().ok()?.take()
    }

    pub async fn spawn(
        &self,
        session_id: &str,
        work: BackgroundWork,
        runtime: BackgroundRuntime,
    ) -> Result<String, String> {
        if work.branches.is_empty() {
            return Err("background task requires at least one branch".into());
        }
        if work.branches.len() > MAX_PARALLEL_BRANCHES {
            return Err(format!(
                "background task exceeds {MAX_PARALLEL_BRANCHES} parallel branches"
            ));
        }
        let mut target_keys = std::collections::HashSet::new();
        for branch in &work.branches {
            validate_task(&branch.task)?;
            if branch.branch_id.trim().is_empty() {
                return Err("background branch_id must not be empty".into());
            }
            if !target_keys.insert(branch.target.key()) {
                return Err("background branches must use distinct delivery targets".into());
            }
        }

        let task_id = uuid::Uuid::new_v4().to_string();
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let created_at = Utc::now();
        let cancel = CancellationToken::new();
        self.tasks.lock().await.insert(
            task_id.clone(),
            BackgroundTaskEntry {
                session_id: session_id.to_string(),
                objective: work.objective.clone(),
                sequence,
                created_at,
                state: "running",
                cancel: cancel.clone(),
            },
        );

        let task_id_for_worker = task_id.clone();
        let session_id = session_id.to_string();
        let branch_count = work.branches.len();
        let event_tx = self.event_tx.clone();
        let tasks = Arc::clone(&self.tasks);
        let branch_slots = Arc::clone(&self.branch_slots);
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            tracing::info!(
                task_id = %task_id_for_worker,
                task_sequence = sequence,
                session = %session_id,
                branches = branch_count,
                created_at = %created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
                "background task started"
            );
            let execution = run_background_work(
                work.clone(),
                runtime,
                branch_slots,
                cancel.clone(),
                shutdown,
            );
            let (branches, cancelled) = tokio::select! {
                result = execution => (result, false),
                _ = cancel.cancelled() => (cancelled_results(&work.branches), true),
            };
            let completed_at = Utc::now();
            if let Some(entry) = tasks.lock().await.get_mut(&task_id_for_worker) {
                entry.state = if cancelled { "cancelled" } else { "completed" };
            }
            tracing::info!(
                task_id = %task_id_for_worker,
                task_sequence = sequence,
                session = %session_id,
                cancelled,
                completed_at = %completed_at.to_rfc3339_opts(SecondsFormat::Millis, true),
                elapsed_ms = completed_at.signed_duration_since(created_at).num_milliseconds(),
                "background task finished"
            );
            let _ = event_tx.send(BackgroundCompletion {
                sequence,
                task_id: task_id_for_worker,
                session_id,
                objective: work.objective,
                created_at,
                completed_at,
                branches,
                cancelled,
            });
        });

        Ok(json!({
            "task_id": task_id,
            "sequence": sequence,
            "status": "running",
            "created_at": created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "branches": branch_count
        })
        .to_string())
    }

    pub async fn list(&self, session_id: &str) -> String {
        let tasks = self.tasks.lock().await;
        let mut rows = tasks
            .iter()
            .filter(|(_, task)| task.session_id == session_id)
            .map(|(task_id, task)| {
                json!({
                    "task_id": task_id,
                    "sequence": task.sequence,
                    "objective": task.objective,
                    "state": task.state,
                    "created_at": task.created_at.to_rfc3339_opts(SecondsFormat::Millis, true)
                })
            })
            .collect::<Vec<_>>();
        rows.sort_by_key(|row| row["sequence"].as_u64().unwrap_or_default());
        json!({ "background_tasks": rows }).to_string()
    }

    pub async fn cancel(&self, session_id: &str, arguments: Value) -> Result<String, String> {
        let args: CancelBackgroundTaskArgs = serde_json::from_value(arguments)
            .map_err(|error| format!("invalid cancel arguments: {error}"))?;
        let tasks = self.tasks.lock().await;
        let Some(task) = tasks.get(&args.task_id) else {
            return Err("background task not found".into());
        };
        if task.session_id != session_id {
            return Err("background task does not belong to the current session".into());
        }
        if task.state != "running" {
            return Err(format!("background task is already {}", task.state));
        }
        task.cancel.cancel();
        Ok(json!({ "task_id": args.task_id, "status": "cancelling" }).to_string())
    }

    pub async fn cancel_running_for_session(&self, session_id: &str, all: bool) -> usize {
        let tasks = self.tasks.lock().await;
        let mut running = tasks
            .values()
            .filter(|task| task.session_id == session_id && task.state == "running")
            .map(|task| (task.sequence, task.cancel.clone()))
            .collect::<Vec<_>>();
        running.sort_by_key(|(sequence, _)| *sequence);
        if !all {
            running = running.into_iter().rev().take(1).collect();
        }
        let count = running.len();
        for (_, cancel) in running {
            cancel.cancel();
        }
        count
    }

    pub async fn mark_integrated(&self, task_id: &str) {
        let mut tasks = self.tasks.lock().await;
        if let Some(task) = tasks.get_mut(task_id) {
            task.state = "integrated";
        }
        if tasks.len() > MAX_BACKGROUND_TASK_HISTORY {
            let mut removable = tasks
                .iter()
                .filter(|(_, task)| task.state != "running")
                .map(|(id, task)| (task.sequence, id.clone()))
                .collect::<Vec<_>>();
            removable.sort_by_key(|(sequence, _)| *sequence);
            let remove_count = tasks.len().saturating_sub(MAX_BACKGROUND_TASK_HISTORY);
            for (_, id) in removable.into_iter().take(remove_count) {
                tasks.remove(&id);
            }
        }
    }
}

async fn run_background_work(
    work: BackgroundWork,
    runtime: BackgroundRuntime,
    branch_slots: Arc<Semaphore>,
    cancel: CancellationToken,
    shutdown: CancellationToken,
) -> Vec<BackgroundBranchResult> {
    let mut handles = Vec::with_capacity(work.branches.len());
    for branch in work.branches {
        let panic_fallback = branch.clone();
        let provider = Arc::clone(&runtime.provider);
        let tools = Arc::clone(&runtime.tools);
        let model = runtime.model.clone();
        let history = runtime.history_snapshot.clone();
        let slots = Arc::clone(&branch_slots);
        let branch_cancel = cancel.clone();
        let process_cancel = shutdown.clone();
        let max_iterations = runtime
            .max_tool_iterations
            .clamp(1, MAX_BACKGROUND_TOOL_ITERATIONS);
        handles.push((
            panic_fallback,
            tokio::spawn(async move {
                let permit = tokio::select! {
                    permit = slots.acquire_owned() => permit.ok(),
                    _ = branch_cancel.cancelled() => None,
                    _ = process_cancel.cancelled() => None,
                };
                let Some(_permit) = permit else {
                    return cancelled_result(branch);
                };
                run_background_branch(
                    branch,
                    provider,
                    tools,
                    model,
                    history,
                    max_iterations,
                    runtime.tool_timeout,
                    branch_cancel,
                    process_cancel,
                )
                .await
            }),
        ));
    }

    let mut results = Vec::with_capacity(handles.len());
    for (fallback, handle) in handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(error) => {
                tracing::warn!(%error, branch_id = %fallback.branch_id, "background branch task panicked");
                let now = Utc::now();
                results.push(BackgroundBranchResult {
                    branch_id: fallback.branch_id,
                    target: fallback.target,
                    success: false,
                    result: format!("background branch panicked: {error}"),
                    started_at: now,
                    completed_at: now,
                });
            }
        }
    }
    results
}

#[allow(clippy::too_many_arguments)]
async fn run_background_branch(
    branch: BackgroundBranch,
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    model: String,
    history: Vec<ChatMessage>,
    max_iterations: usize,
    tool_timeout: Duration,
    cancel: CancellationToken,
    shutdown: CancellationToken,
) -> BackgroundBranchResult {
    let started_at = Utc::now();
    let mut messages = vec![ChatMessage::system(
        "You are a background subagent working from a point-in-time conversation snapshot. Complete the assigned task independently. You may use available non-messaging tools. Never send platform messages, change adapter state, schedule work, or claim that the parent has already delivered your result. Return a factual result for ordered integration by the parent agent.",
    )];
    messages.extend(history);
    if let Some(context) = &branch.context {
        messages.push(ChatMessage::user(format!("Branch context:\n{context}")));
    }
    messages.push(ChatMessage::user(format!("Branch task:\n{}", branch.task)));
    let definitions = tools
        .definitions()
        .await
        .iter()
        .filter(|definition| background_tool_allowed(&definition.name))
        .cloned()
        .collect::<Vec<_>>();

    let result = async {
        for _ in 0..max_iterations {
            let request = ChatRequest {
                model: model.clone(),
                messages: messages.clone(),
                tools: Some(definitions.clone()),
                temperature: None,
                max_tokens: None,
            };
            let response = provider
                .chat(&request)
                .await
                .map_err(|error| format!("background model request failed: {error}"))?;
            if response.tool_calls.is_empty() {
                let content = response.content.unwrap_or_default();
                if content.trim().is_empty() {
                    return Err("background branch returned an empty result".into());
                }
                return Ok(content);
            }
            messages.push(super::assistant_with_tool_calls(
                &response.tool_calls,
                &response.content,
                &response.reasoning_content,
            ));
            for call in response.tool_calls {
                if !background_tool_allowed(&call.name) {
                    messages.push(ChatMessage::tool(
                        "error: tool is not available to background branches",
                        call.id,
                    ));
                    continue;
                }
                let arguments = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
                let output = tokio::select! {
                    result = tools.execute(&call.name, arguments) => result
                        .map_err(|error| error.to_string())
                        .unwrap_or_else(|error| format!("error: {error}")),
                    _ = tokio::time::sleep(tool_timeout) => {
                        // 与主循环一致：超时以 notice（非 error 前缀）喂回模型，
                        // 分支可重试或用已有信息继续作答，不视为致命失败。
                        format!(
                            "notice: tool '{}' timed out after {}s and its execution was aborted. You may retry this tool (e.g. with a shorter command) or continue the answer directly with the information you already have; do not treat this timeout as a fatal failure.",
                            call.name,
                            tool_timeout.as_secs(),
                        )
                    }
                };
                messages.push(ChatMessage::tool(output, call.id));
            }
        }
        Err(format!(
            "background branch reached its tool iteration limit ({max_iterations})"
        ))
    };

    let outcome = tokio::select! {
        result = result => result,
        _ = cancel.cancelled() => Err("background branch cancelled".into()),
        _ = shutdown.cancelled() => Err("background branch stopped during shutdown".into()),
    };
    let completed_at = Utc::now();
    match outcome {
        Ok(result) => BackgroundBranchResult {
            branch_id: branch.branch_id,
            target: branch.target,
            success: true,
            result,
            started_at,
            completed_at,
        },
        Err(error) => BackgroundBranchResult {
            branch_id: branch.branch_id,
            target: branch.target,
            success: false,
            result: error,
            started_at,
            completed_at,
        },
    }
}

fn background_tool_allowed(name: &str) -> bool {
    !matches!(
        name,
        "send_private_msg"
            | "send_group_msg"
            | "send_like"
            | "upload_group_file"
            | "upload_private_file"
            | "start_adapter"
            | "stop_adapter"
            | "restart_adapter"
            | "run_sudo" // interactive sudo prompts must not fire from detached branches
    )
}

fn cancelled_results(branches: &[BackgroundBranch]) -> Vec<BackgroundBranchResult> {
    branches.iter().cloned().map(cancelled_result).collect()
}

fn cancelled_result(branch: BackgroundBranch) -> BackgroundBranchResult {
    let now = Utc::now();
    BackgroundBranchResult {
        branch_id: branch.branch_id,
        target: branch.target,
        success: false,
        result: "background branch cancelled".into(),
        started_at: now,
        completed_at: now,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleTimerArgs {
    task: String,
    #[serde(default)]
    delay_seconds: Option<u64>,
    #[serde(default)]
    run_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelTimerArgs {
    timer_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SubagentArgs {
    pub task: String,
    #[serde(default)]
    pub context: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnBackgroundTaskArgs {
    task: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    target: Option<DeliveryTargetArgs>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnParallelTaskArgs {
    objective: String,
    branches: Vec<ParallelBranchArgs>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParallelBranchArgs {
    branch_id: String,
    task: String,
    #[serde(default)]
    context: Option<String>,
    target: DeliveryTargetArgs,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveryTargetArgs {
    kind: String,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelBackgroundTaskArgs {
    task_id: String,
}

pub(super) fn parse_background_task_args(
    arguments: Value,
    origin_session_id: &str,
) -> Result<BackgroundWork, String> {
    let args: SpawnBackgroundTaskArgs = serde_json::from_value(arguments)
        .map_err(|error| format!("invalid background task arguments: {error}"))?;
    validate_task(&args.task)?;
    validate_optional_context(args.context.as_deref())?;
    let target = resolve_delivery_target(args.target.as_ref(), origin_session_id)?;
    Ok(BackgroundWork {
        objective: args.task.clone(),
        branches: vec![BackgroundBranch {
            branch_id: "main".into(),
            task: args.task,
            context: args.context,
            target,
        }],
    })
}

pub(super) fn parse_parallel_task_args(
    arguments: Value,
    origin_session_id: &str,
) -> Result<BackgroundWork, String> {
    let args: SpawnParallelTaskArgs = serde_json::from_value(arguments)
        .map_err(|error| format!("invalid parallel task arguments: {error}"))?;
    validate_task(&args.objective)?;
    if args.branches.is_empty() {
        return Err("parallel task requires at least one branch".into());
    }
    if args.branches.len() > MAX_PARALLEL_BRANCHES {
        return Err(format!(
            "parallel task exceeds {MAX_PARALLEL_BRANCHES} branches"
        ));
    }
    let mut branch_ids = std::collections::HashSet::new();
    let mut branches = Vec::with_capacity(args.branches.len());
    for branch in args.branches {
        validate_task(&branch.task)?;
        validate_optional_context(branch.context.as_deref())?;
        if !branch_ids.insert(branch.branch_id.clone()) {
            return Err(format!("duplicate branch_id: {}", branch.branch_id));
        }
        branches.push(BackgroundBranch {
            branch_id: branch.branch_id,
            task: branch.task,
            context: branch.context,
            target: resolve_delivery_target(Some(&branch.target), origin_session_id)?,
        });
    }
    Ok(BackgroundWork {
        objective: args.objective,
        branches,
    })
}

pub(super) fn parse_subagent_args(arguments: Value) -> Result<SubagentArgs, String> {
    let args: SubagentArgs = serde_json::from_value(arguments)
        .map_err(|error| format!("invalid subagent arguments: {error}"))?;
    validate_task(&args.task)?;
    if args
        .context
        .as_ref()
        .is_some_and(|context| context.chars().count() > MAX_TASK_CHARS)
    {
        return Err(format!(
            "subagent context exceeds {MAX_TASK_CHARS} characters"
        ));
    }
    Ok(args)
}

fn validate_optional_context(context: Option<&str>) -> Result<(), String> {
    if context.is_some_and(|context| context.chars().count() > MAX_TASK_CHARS) {
        return Err(format!(
            "background context exceeds {MAX_TASK_CHARS} characters"
        ));
    }
    Ok(())
}

fn resolve_delivery_target(
    target: Option<&DeliveryTargetArgs>,
    origin_session_id: &str,
) -> Result<DeliveryTarget, String> {
    let origin = SessionKey::parse(origin_session_id)
        .ok_or_else(|| "origin session is invalid".to_string())?;
    let kind = target
        .map(|target| target.kind.as_str())
        .unwrap_or("origin");
    let explicit_id = target.and_then(|target| target.id.as_deref());
    match kind {
        "origin" if origin.platform.eq_ignore_ascii_case("qq") && origin.scope == "dm" => {
            Ok(DeliveryTarget::QqPrivate {
                user_id: origin.user_id,
            })
        }
        "origin" if origin.platform.eq_ignore_ascii_case("qq") && origin.scope == "group" => {
            Ok(DeliveryTarget::QqGroup {
                group_id: origin.scope_id,
            })
        }
        "origin" => Ok(DeliveryTarget::Backend {
            session_id: origin_session_id.to_string(),
        }),
        "backend" => Ok(DeliveryTarget::Backend {
            session_id: explicit_id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or(origin_session_id)
                .to_string(),
        }),
        "qq_private" => Ok(DeliveryTarget::QqPrivate {
            user_id: required_target_id(explicit_id, "qq_private")?,
        }),
        "qq_group" => Ok(DeliveryTarget::QqGroup {
            group_id: required_target_id(explicit_id, "qq_group")?,
        }),
        other => Err(format!(
            "unsupported delivery target kind: {other}; expected origin/backend/qq_private/qq_group"
        )),
    }
}

fn required_target_id(value: Option<&str>, kind: &str) -> Result<String, String> {
    value
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("delivery target {kind} requires id"))
}

fn validate_task(task: &str) -> Result<(), String> {
    if task.trim().is_empty() {
        return Err("task must not be empty".into());
    }
    if task.chars().count() > MAX_TASK_CHARS {
        return Err(format!("task exceeds {MAX_TASK_CHARS} characters"));
    }
    Ok(())
}

fn resolve_due_at(args: &ScheduleTimerArgs) -> Result<DateTime<Utc>, String> {
    match (args.delay_seconds, args.run_at.as_deref()) {
        (Some(_), Some(_)) => Err("provide exactly one of delay_seconds or run_at".into()),
        (None, None) => Err("provide exactly one of delay_seconds or run_at".into()),
        (Some(seconds), None) => Ok(Utc::now()
            + chrono::Duration::from_std(Duration::from_secs(seconds))
                .map_err(|_| "delay_seconds is too large")?),
        (None, Some(value)) => {
            let due_at = DateTime::parse_from_rfc3339(value)
                .map_err(|_| "run_at must be RFC 3339 with a timezone offset")?
                .with_timezone(&Utc);
            if due_at < Utc::now() {
                return Err("run_at must not be in the past".into());
            }
            Ok(due_at)
        }
    }
}

/// The stable set of orchestration tool names, in schema order. Both the
/// schema generation (`tool_definitions`) and the execution dispatch
/// (`Agent::run_tool`) derive from this single table, so a new orchestration
/// tool cannot drift between its schema and its handler.
pub(super) const ORCHESTRATION_TOOL_NAMES: &[&str] = &[
    "schedule_timer",
    "list_timers",
    "cancel_timer",
    "run_subagent",
    "spawn_background_task",
    "spawn_parallel_task",
    "list_background_tasks",
    "cancel_background_task",
    "send_backend_message",
];

/// Orchestration tool category shown in the Panel capability editor.
pub const ORCHESTRATION_CATEGORY: &str = "编排";

/// Tool metadata (name/description/category) for the frontend tool list.
/// `tool_definitions` builds the LLM schemas from the same names, so the
/// panel list can never drift from what the agent actually exposes.
pub fn orchestration_tool_meta() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "schedule_timer",
            "安排定时任务，到点以 <timer_event> 回投（进程内，重启丢失）",
            ORCHESTRATION_CATEGORY,
        ),
        (
            "list_timers",
            "列出当前会话的待执行定时任务",
            ORCHESTRATION_CATEGORY,
        ),
        (
            "cancel_timer",
            "取消一个待执行的定时任务",
            ORCHESTRATION_CATEGORY,
        ),
        (
            "run_subagent",
            "启动一个有边界的子代理执行推理任务，返回结果给宿主",
            ORCHESTRATION_CATEGORY,
        ),
        (
            "spawn_background_task",
            "启动长期运行的后台任务分支（不阻塞宿主）",
            ORCHESTRATION_CATEGORY,
        ),
        (
            "spawn_parallel_task",
            "并发启动多个独立分支并汇总结果",
            ORCHESTRATION_CATEGORY,
        ),
        (
            "list_background_tasks",
            "列出当前会话的后台任务状态",
            ORCHESTRATION_CATEGORY,
        ),
        (
            "cancel_background_task",
            "取消一个运行中的后台任务",
            ORCHESTRATION_CATEGORY,
        ),
        (
            "send_backend_message",
            "向某个后端/TUI 会话投递消息（structured event 交付用）",
            ORCHESTRATION_CATEGORY,
        ),
        (
            "framework_update",
            "查询或触发框架自更新（需授权）",
            "自更新",
        ),
        (
            "run_sudo",
            "通过 sudo 以 root 执行命令（需用户在 Panel 输入密码）",
            "运维",
        ),
    ]
}

pub(super) fn tool_definitions(
    self_update_enabled: bool,
    sudo_enabled: bool,
) -> Vec<ToolDefinition> {
    let mut definitions = vec![
        ToolDefinition {
            name: "schedule_timer".into(),
            description: "Schedule a task for this conversation using exactly one of delay_seconds or run_at. When due, the task returns as a backend <timer_event>; use an explicit platform send tool if it must be delivered to QQ. Timers are in-memory and are lost when the process stops.".into(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "What the agent should do when the timer fires" },
                    "delay_seconds": { "type": "integer", "minimum": 0, "maximum": 2678400, "description": "Delay from now in seconds" },
                    "run_at": { "type": "string", "description": "Absolute RFC 3339 time with timezone, for example 2030-01-02T20:00:00+08:00" }
                },
                "required": ["task"],
                "additionalProperties": false
            })),
        },
        ToolDefinition {
            name: "list_timers".into(),
            description: "List pending timers created in the current conversation.".into(),
            parameters: Some(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
        },
        ToolDefinition {
            name: "cancel_timer".into(),
            description: "Cancel a pending timer in the current conversation.".into(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "timer_id": { "type": "string" }
                },
                "required": ["timer_id"],
                "additionalProperties": false
            })),
        },
        ToolDefinition {
            name: "run_subagent".into(),
            description: "Run one isolated subagent for a bounded research or reasoning task. It has no tools and cannot send platform messages; its result is returned privately to the parent agent.".into(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "A complete, bounded task for the subagent" },
                    "context": { "type": "string", "description": "Only the background information the subagent needs" }
                },
                "required": ["task"],
                "additionalProperties": false
            })),
        },
        ToolDefinition {
            name: "spawn_background_task".into(),
            description: "Start a long-running subagent task in the background and return immediately with a task_id. The background branch receives a point-in-time conversation snapshot and non-messaging tools. Its result is later integrated into the shared context in task creation order, then explicitly delivered to origin or the requested target.".into(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "Complete, self-contained long-running task" },
                    "context": { "type": "string", "description": "Additional branch-only context" },
                    "target": { "$ref": "#/$defs/target" }
                },
                "required": ["task"],
                "additionalProperties": false,
                "$defs": { "target": delivery_target_schema() }
            })),
        },
        ToolDefinition {
            name: "spawn_parallel_task".into(),
            description: "Start 1-8 independent temporary subagent branches concurrently and return immediately. Each branch has one distinct backend, QQ private, or QQ group delivery target. Results are collected in declared branch order and integrated into shared context according to the parent task creation sequence.".into(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "objective": { "type": "string", "description": "Overall objective used when integrating branch results" },
                    "branches": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_PARALLEL_BRANCHES,
                        "items": {
                            "type": "object",
                            "properties": {
                                "branch_id": { "type": "string" },
                                "task": { "type": "string" },
                                "context": { "type": "string" },
                                "target": { "$ref": "#/$defs/target" }
                            },
                            "required": ["branch_id", "task", "target"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["objective", "branches"],
                "additionalProperties": false,
                "$defs": { "target": delivery_target_schema() }
            })),
        },
        ToolDefinition {
            name: "list_background_tasks".into(),
            description: "List background tasks created by the current source session, including stable creation sequence and state.".into(),
            parameters: Some(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
        },
        ToolDefinition {
            name: "cancel_background_task".into(),
            description: "Cancel a running background task created by the current source session.".into(),
            parameters: Some(json!({
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"],
                "additionalProperties": false
            })),
        },
        ToolDefinition {
            name: "send_backend_message".into(),
            description: "Deliver a message to a specific backend/TUI session. Use this only when a structured background_task_event requires a backend delivery; normal assistant output remains the response for ordinary backend input.".into(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["session_id", "content"],
                "additionalProperties": false
            })),
        },
    ];
    if self_update_enabled {
        definitions.push(ToolDefinition {
            name: "framework_update".into(),
            description: "Inspect or trigger the installed EchoAgentCore self-update service. Use action=status to inspect it. Use action=apply with confirm=true only after an authorized user explicitly asks to update the framework. The update runs asynchronously and restarts Core after a successful atomic binary replacement; this tool cannot run arbitrary commands.".into(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["status", "apply"] },
                    "confirm": { "type": "boolean", "description": "Must be true for action=apply" }
                },
                "required": ["action"],
                "additionalProperties": false
            })),
        });
    }
    if sudo_enabled {
        definitions.push(ToolDefinition {
            name: "run_sudo".into(),
            description: "Run a shell command with root privileges via sudo. The user is prompted in the Panel to authorize with their password; you never see or handle the password, so never ask for it and never try to guess it. Use only when elevated privileges are genuinely required. Returns stdout and stderr. Timeout for user authorization: 120s; command timeout: 60s.".into(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to run with sudo (e.g. 'apt-get update', 'systemctl restart nginx')" }
                },
                "required": ["command"],
                "additionalProperties": false
            })),
        });
    }
    definitions
}

fn delivery_target_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["origin", "backend", "qq_private", "qq_group"]
            },
            "id": {
                "type": "string",
                "description": "Required for qq_private/qq_group; backend defaults to the origin session"
            }
        },
        "required": ["kind"],
        "additionalProperties": false
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrameworkUpdateArgs {
    action: FrameworkUpdateAction,
    #[serde(default)]
    confirm: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FrameworkUpdateAction {
    Status,
    Apply,
}

pub(super) async fn framework_update(
    config: &SelfUpdateConfig,
    session_id: &str,
    arguments: Value,
) -> Result<String, String> {
    if !config.enabled {
        return Err("framework self-update is disabled".into());
    }
    if !self_update_authorized(config, session_id) {
        return Err("current session is not authorized to update the framework".into());
    }
    let args: FrameworkUpdateArgs = serde_json::from_value(arguments)
        .map_err(|error| format!("invalid framework update arguments: {error}"))?;

    match args.action {
        FrameworkUpdateAction::Status => framework_update_status().await,
        FrameworkUpdateAction::Apply => {
            if !args.confirm {
                return Err("apply requires confirm=true after an explicit user request".into());
            }
            start_framework_update().await
        }
    }
}

fn self_update_authorized(config: &SelfUpdateConfig, session_id: &str) -> bool {
    let Some(session) = crate::session::SessionKey::parse(session_id) else {
        return false;
    };
    if session.platform == "local" && session.scope == "tui" {
        return config.allow_local;
    }
    if session.platform != "qq" {
        return false;
    }
    session
        .user_id
        .parse::<u64>()
        .ok()
        .is_some_and(|id| config.allowed_qq_users.contains(&id))
}

async fn start_framework_update() -> Result<String, String> {
    let load_state = tokio::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=LoadState",
            "--value",
            UPDATE_SERVICE,
        ])
        .output()
        .await
        .map_err(|error| format!("cannot invoke systemctl: {error}"))?;
    if !load_state.status.success()
        || String::from_utf8_lossy(&load_state.stdout).trim() != "loaded"
    {
        return Err(format!(
            "{UPDATE_SERVICE} is not installed; run scripts/install.sh first"
        ));
    }

    let output = tokio::process::Command::new("systemctl")
        .args(["--user", "start", "--no-block", UPDATE_SERVICE])
        .output()
        .await
        .map_err(|error| format!("cannot start update service: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "update service start failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(json!({
        "status": "scheduled",
        "service": UPDATE_SERVICE,
        "message": "update runs in the background; Core restarts only after a successful build"
    })
    .to_string())
}

async fn framework_update_status() -> Result<String, String> {
    let output = tokio::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=LoadState,ActiveState,SubState,Result",
            UPDATE_SERVICE,
        ])
        .output()
        .await
        .map_err(|error| format!("cannot inspect update service: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot inspect update service: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let service = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let status_file = std::env::var("ECHO_UPDATE_STATUS_FILE")
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_else(|| "state=never_run".into());
    // A failed service must not coexist with a stale non-terminal status file
    // (e.g. left at "running" by an interrupted run). Flag the contradiction so
    // callers can distinguish a real failure from a stuck status file.
    let service_failed =
        service.contains("ActiveState=failed") || service.contains("SubState=failed");
    let status_terminal = status_file.lines().any(|line| {
        line.starts_with("state=failed")
            || line.starts_with("state=rolled_back")
            || line.starts_with("state=updated")
            || line.starts_with("state=current")
            || line.starts_with("state=interrupted")
    });
    // 附带插件摘要（内置插件清单快照），供 Panel / LLM 展示更新后的能力面。
    let plugins = gather_plugin_summary();
    Ok(json!({
        "service": service,
        "update": status_file.trim(),
        "stale_status": service_failed && !status_terminal,
        "plugins": plugins
    })
    .to_string())
}

/// Snapshot of registered plugins (id/version/kind/enabled), gathered from
/// the agent's plugin host when available, or an empty list (e.g. tests).
fn gather_plugin_summary() -> Vec<serde_json::Value> {
    // The orchestration module runs inside the agent; the plugin host is
    // available through the process-wide (best-effort) atomic registration
    // set up by the composition root.
    let Some(host) = crate::agent::plugin_host_global() else {
        return Vec::new();
    };
    host.descriptors()
        .into_iter()
        .map(|d| {
            json!({
                "id": d.id,
                "version": d.version,
                "kind": d.kind,
                "enabled": d.enabled
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orchestration_tool_names_are_stable() {
        let names: Vec<_> = tool_definitions(false, false)
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert_eq!(
            names, ORCHESTRATION_TOOL_NAMES,
            "schema order matches the single dispatch table"
        );
        // Every schema name must have a handler in the same table.
        assert_eq!(names.len(), ORCHESTRATION_TOOL_NAMES.len());
    }

    #[test]
    fn self_update_tool_is_opt_in() {
        assert!(!tool_definitions(false, false)
            .iter()
            .any(|definition| definition.name == "framework_update"));
        assert!(tool_definitions(true, false)
            .iter()
            .any(|definition| definition.name == "framework_update"));
    }

    #[test]
    fn sudo_tool_is_opt_in() {
        assert!(!tool_definitions(false, false)
            .iter()
            .any(|definition| definition.name == "run_sudo"));
        assert!(tool_definitions(false, true)
            .iter()
            .any(|definition| definition.name == "run_sudo"));
    }

    #[test]
    fn self_update_authorization_is_session_scoped() {
        let config = SelfUpdateConfig {
            enabled: true,
            allow_local: true,
            allowed_qq_users: vec![12345],
        };
        assert!(self_update_authorized(&config, "local:tui::local_user"));
        assert!(self_update_authorized(&config, "qq:dm::12345"));
        assert!(self_update_authorized(&config, "qq:group:99:12345"));
        assert!(!self_update_authorized(&config, "qq:dm::99999"));
        assert!(!self_update_authorized(&config, "timer:tui::12345"));
    }

    #[tokio::test]
    async fn self_update_apply_requires_confirmation_before_systemctl() {
        let config = SelfUpdateConfig {
            enabled: true,
            allow_local: true,
            allowed_qq_users: Vec::new(),
        };
        let error = framework_update(
            &config,
            "local:tui::local_user",
            json!({ "action": "apply", "confirm": false }),
        )
        .await
        .unwrap_err();
        assert!(error.contains("confirm=true"));
    }

    #[tokio::test]
    async fn unauthorized_update_is_rejected_before_status_lookup() {
        let config = SelfUpdateConfig {
            enabled: true,
            allow_local: false,
            allowed_qq_users: vec![12345],
        };
        let error = framework_update(&config, "qq:dm::99999", json!({ "action": "status" }))
            .await
            .unwrap_err();
        assert!(error.contains("not authorized"));
    }

    #[test]
    fn timer_requires_one_time_mode() {
        let neither = ScheduleTimerArgs {
            task: "x".into(),
            delay_seconds: None,
            run_at: None,
        };
        assert!(resolve_due_at(&neither).is_err());
        let both = ScheduleTimerArgs {
            task: "x".into(),
            delay_seconds: Some(1),
            run_at: Some("2026-08-06T20:00:00+08:00".into()),
        };
        assert!(resolve_due_at(&both).is_err());
    }

    fn completion(sequence: u64) -> BackgroundCompletion {
        BackgroundCompletion {
            sequence,
            task_id: format!("task-{sequence}"),
            session_id: "local:tui::local_user".into(),
            objective: format!("objective-{sequence}"),
            created_at: Utc::now(),
            completed_at: Utc::now(),
            branches: Vec::new(),
            cancelled: false,
        }
    }

    #[test]
    fn background_completions_are_released_in_creation_order() {
        let mut ordered = OrderedCompletionBuffer::new();
        assert!(ordered.push(completion(2)).is_empty());
        let ready = ordered.push(completion(1));
        assert_eq!(
            ready
                .iter()
                .map(|completion| completion.sequence)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(ordered.push(completion(3))[0].sequence, 3);
    }

    #[test]
    fn background_target_defaults_to_origin_and_parallel_targets_are_explicit() {
        let work =
            parse_background_task_args(json!({ "task": "research" }), "qq:group:987:123").unwrap();
        assert_eq!(
            work.branches[0].target,
            DeliveryTarget::QqGroup {
                group_id: "987".into()
            }
        );

        let work = parse_parallel_task_args(
            json!({
                "objective": "reply to two places",
                "branches": [
                    {
                        "branch_id": "dm",
                        "task": "draft dm",
                        "target": { "kind": "qq_private", "id": "100" }
                    },
                    {
                        "branch_id": "backend",
                        "task": "draft backend",
                        "target": { "kind": "backend", "id": "local:tui::local_user" }
                    }
                ]
            }),
            "local:tui::local_user",
        )
        .unwrap();
        assert_eq!(work.branches.len(), 2);
        assert_eq!(
            work.branches[1].target,
            DeliveryTarget::Backend {
                session_id: "local:tui::local_user".into()
            }
        );
    }

    struct SlowProvider;

    #[async_trait::async_trait]
    impl LlmProvider for SlowProvider {
        fn name(&self) -> &str {
            "slow"
        }

        fn default_model(&self) -> &str {
            "slow"
        }

        async fn chat(
            &self,
            _request: &ChatRequest,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(crate::llm::ChatResponse {
                stop_reason: None,
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: crate::llm::Usage::default(),
            })
        }

        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<crate::llm::ChatChunk>,
        ) -> Result<(), crate::llm::LlmError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn spawning_background_work_returns_before_the_provider_finishes() {
        let shutdown = CancellationToken::new();
        let manager = BackgroundTaskManager::new(shutdown.clone());
        let started = tokio::time::Instant::now();
        let receipt = manager
            .spawn(
                "local:tui::local_user",
                BackgroundWork {
                    objective: "slow task".into(),
                    branches: vec![BackgroundBranch {
                        branch_id: "main".into(),
                        task: "slow task".into(),
                        context: None,
                        target: DeliveryTarget::Backend {
                            session_id: "local:tui::local_user".into(),
                        },
                    }],
                },
                BackgroundRuntime {
                    provider: Arc::new(SlowProvider),
                    tools: Arc::new(ToolRegistry::new()),
                    model: "slow".into(),
                    max_tool_iterations: 2,
                    tool_timeout: Duration::from_secs(120),
                    history_snapshot: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(
            serde_json::from_str::<Value>(&receipt).unwrap()["status"],
            "running"
        );
        shutdown.cancel();
    }
}
