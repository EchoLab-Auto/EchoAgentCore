//! The agent loop: LLM + tools + skills + memory composed into one reply.

mod commands;
mod orchestration;
mod qq_commands;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use tokio::sync::{Mutex, RwLock};

use echo_adapter::AdapterRegistry;

use crate::bridge::BackendHandle;
use crate::command::BackendCommand;
use crate::config::{AgentConfig, ApiProfile};
use crate::event::{AdapterStatus, BackendEvent};
use crate::llm::{create_provider, ChatMessage, ChatRequest, LlmProvider, ToolCall};
use crate::session::{Session, TrunkStore};
use crate::skill::SkillRegistry;
use crate::tool::ToolRegistry;
use echo_chat_capability::{DeliveryPolicy, DeliveryTarget};

const TURN_CANCELLED: &str = "agent turn cancelled by requester";
const MAX_CONCURRENT_REPLY_BRANCHES: usize = 8;
/// Upper bound on concurrent "wait reply" generation calls. Every long-running
/// QQ branch spawns a delayed interim reply; without a cap, many parallel
/// branches would each fire an extra LLM request at the same moment.
const MAX_CONCURRENT_WAIT_REPLIES: usize = 4;

#[derive(Debug, Clone)]
struct ActiveInboundTurn {
    session_id: String,
    message_sequence: u64,
    branch: bool,
    cancel: tokio_util::sync::CancellationToken,
}

pub(crate) struct InboundTurnRegistration {
    pub id: String,
    pub cancel: tokio_util::sync::CancellationToken,
}

/// The agent: owns provider, tools, skills, sessions, adapters, and emits events.
pub struct Agent {
    provider: RwLock<Arc<dyn LlmProvider>>,
    config: RwLock<AgentConfig>,
    active_model: RwLock<String>,
    /// Shared config store for persisting `[agent]` changes (optional).
    config_store: tokio::sync::Mutex<Option<echo_adapter::ConfigStore>>,
    pub tools: Arc<ToolRegistry>,
    pub skills: Arc<Mutex<SkillRegistry>>,
    pub trunk: TrunkStore,
    /// Platform adapters managed externally (registered by main / config).
    pub adapters: Arc<AdapterRegistry>,
    /// Tokio RwLock: `emit()` runs from async loops and must never block a
    /// worker thread. All access goes through `try_*` (never held across
    /// await points, contention is negligible).
    pub handle: tokio::sync::RwLock<Option<Arc<BackendHandle>>>,
    /// Human-in-the-loop sudo authorization broker (set by the composition
    /// root). `run_sudo` awaits a password submitted on the dedicated sudo
    /// channel; see [`crate::sudo`].
    pub sudo_broker: tokio::sync::RwLock<Option<Arc<crate::sudo::SudoBroker>>>,
    system_prompt_cache: RwLock<Option<String>>,
    /// Decomposed system-prompt blocks of the most recent turn, kept so the
    /// panel can visualize the exact prompt sections sent to the LLM.
    last_prompt_blocks: tokio::sync::Mutex<Option<Vec<PromptBlock>>>,
    skill_reload_started: AtomicBool,
    orchestration_started: AtomicBool,
    timer_scheduler: orchestration::TimerScheduler,
    background_tasks: orchestration::BackgroundTaskManager,
    reply_branch_slots: tokio::sync::Semaphore,
    /// Bounds concurrent interim "wait reply" LLM calls (see
    /// [`MAX_CONCURRENT_WAIT_REPLIES`]).
    wait_reply_slots: tokio::sync::Semaphore,
    active_inbound_turns: DashMap<String, ActiveInboundTurn>,
    next_message_sequence: AtomicU64,
    /// Event bus: `emit` broadcasts through it; consumers (the timeline
    /// projector, the frontend bridge) subscribe as listeners.
    pub event_bus: std::sync::Arc<echo_context::EventBus>,
    /// Keeps the timeline projector's bus subscription alive for the agent's
    /// lifetime; dropped (disposed) together with the agent.
    _timeline_projection: echo_context::Disposer,
    /// Cancels background tasks (identity eviction / save) on shutdown.
    cancel: tokio_util::sync::CancellationToken,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("active_model", &self.active_model)
            .field("adapters", &self.adapters.names())
            .finish_non_exhaustive()
    }
}

impl Agent {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        config: AgentConfig,
        skills: SkillRegistry,
        tools: ToolRegistry,
        adapters: Arc<AdapterRegistry>,
    ) -> Self {
        // The agent owns exactly ONE conversation context: the trunk. Every
        // inbound message forks a temporary branch from a trunk snapshot and
        // merges back in append-only order (see `process_message`).
        let trunk = TrunkStore::new(config.effective_memory_limit_tokens());
        let eviction_trunk = trunk.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let eviction_cancel = cancel.clone();
        let timer_scheduler = orchestration::TimerScheduler::new(cancel.clone());
        let background_tasks = orchestration::BackgroundTaskManager::new(cancel.clone());
        let event_bus: std::sync::Arc<echo_context::EventBus> =
            std::sync::Arc::new(echo_context::EventBus::default());
        // The display timeline is a projection of emitted events; subscribe
        // the projector as an observe listener so `emit` fans out to it. The
        // returned disposer must be held for the agent's lifetime, or the
        // subscription unwinds immediately.
        let timeline_projector: std::sync::Arc<crate::timeline::TimelineProjector> =
            std::sync::Arc::new(crate::timeline::TimelineProjector::new(trunk.clone()));
        let _timeline_projection = timeline_projector.subscribe(&event_bus);
        let agent = Self {
            provider: RwLock::new(provider),
            config: RwLock::new(config),
            active_model: RwLock::new(String::new()),
            config_store: tokio::sync::Mutex::new(None),
            tools: Arc::new(tools),
            skills: Arc::new(Mutex::new(skills)),
            trunk,
            adapters,
            handle: tokio::sync::RwLock::new(None),
            sudo_broker: tokio::sync::RwLock::new(None),
            system_prompt_cache: RwLock::new(None),
            last_prompt_blocks: tokio::sync::Mutex::new(None),
            skill_reload_started: AtomicBool::new(false),
            orchestration_started: AtomicBool::new(false),
            timer_scheduler,
            background_tasks,
            reply_branch_slots: tokio::sync::Semaphore::new(MAX_CONCURRENT_REPLY_BRANCHES),
            wait_reply_slots: tokio::sync::Semaphore::new(MAX_CONCURRENT_WAIT_REPLIES),
            active_inbound_turns: DashMap::new(),
            next_message_sequence: AtomicU64::new(1),
            event_bus,
            _timeline_projection,
            cancel,
        };
        // Spawn periodic identity eviction (every 5 minutes). Only stale
        // source labels are dropped; the trunk history is never evicted.
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                tokio::select! {
                    _ = interval.tick() => eviction_trunk.evict_idle(),
                    _ = eviction_cancel.cancelled() => break,
                }
            }
        });
        agent
    }

    /// Graceful shutdown: stop background tasks and flush sessions to disk.
    pub async fn shutdown(&self) {
        self.cancel_all_inbound_turns();
        self.cancel.cancel();
        self.trunk.save_now().await;
    }

    /// Set the TOML config file that `[agent]` changes are persisted to.
    pub fn set_config_path(&self, path: impl Into<std::path::PathBuf>) {
        let p: std::path::PathBuf = path.into();
        self.set_config_store(echo_adapter::ConfigStore::new(p));
    }

    /// Attach a shared [`echo_adapter::ConfigStore`] for `[agent]` persistence.
    ///
    /// Also sets the session persistence path (same directory). Called once
    /// at startup — uses try_lock since no concurrent access exists yet.
    pub fn set_config_store(&self, store: echo_adapter::ConfigStore) {
        let sessions_path = store.path().with_file_name("echo-sessions.json");
        self.trunk.set_persist_path(sessions_path);
        if let Ok(mut slot) = self.config_store.try_lock() {
            *slot = Some(store);
        } else {
            tracing::warn!("config store slot busy, ignoring set_config_store");
        }
    }

    /// Load persisted sessions from disk.
    pub async fn load_sessions(&self) -> usize {
        self.trunk.load_from_file().await
    }

    /// Spawn a periodic session save task.
    pub fn start_session_save_task(self: &Arc<Self>) {
        let agent = Arc::clone(self);
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = interval.tick() => agent.trunk.maybe_save().await,
                    _ = cancel.cancelled() => break,
                }
            }
        });
    }

    /// Watch the configured skill directory and reload changed `SKILL.md`
    /// files. Runtime enable/disable choices survive content reloads.
    pub async fn start_skill_reload_task(self: &Arc<Self>) {
        let skills_dir = self.config.read().await.skills_dir.clone();
        if skills_dir.trim().is_empty()
            || self
                .skill_reload_started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }

        let agent = Arc::clone(self);
        let cancel = self.cancel.clone();
        tracing::info!(path = %skills_dir, interval_seconds = 1, "skill hot reload started");
        tokio::spawn(async move {
            let start = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
            let mut interval = tokio::time::interval_at(start, std::time::Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(error) = agent.reload_skills(&skills_dir).await {
                            tracing::warn!(%error, path = %skills_dir, "skill hot reload failed; keeping previous registry");
                        }
                    }
                    _ = cancel.cancelled() => break,
                }
            }
        });
    }

    /// Start delivery of scheduled timer events back into their originating sessions.
    pub fn start_orchestration_task(self: &Arc<Self>) {
        if self
            .orchestration_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let timer_receiver = self.timer_scheduler.take_receiver();
        let background_receiver = self.background_tasks.take_receiver();
        if timer_receiver.is_none() && background_receiver.is_none() {
            self.orchestration_started.store(false, Ordering::Release);
            tracing::warn!("orchestration event receivers unavailable");
            return;
        }

        if let Some(mut receiver) = timer_receiver {
            let agent = Arc::clone(self);
            let cancel = self.cancel.clone();
            tokio::spawn(async move {
                loop {
                    let event = tokio::select! {
                        event = receiver.recv() => match event {
                            Some(event) => event,
                            None => break,
                        },
                        _ = cancel.cancelled() => break,
                    };
                    agent.timer_scheduler.mark_delivered(&event.id).await;
                    let Some(key) = crate::session::SessionKey::parse(&event.session_id) else {
                        tracing::warn!(timer_id = %event.id, session_id = %event.session_id, "timer session is invalid");
                        continue;
                    };
                    let session = agent
                        .trunk
                        .get(&event.session_id)
                        .unwrap_or_else(|| agent.trunk.get_or_create(&key, "timer".into(), None));
                    let received_at_ms = chrono::Utc::now().timestamp_millis();
                    let message_sequence = agent.next_message_sequence();
                    let input = serde_json::json!({
                        "event": "timer",
                        "message_sequence": message_sequence,
                        "received_at_ms": received_at_ms,
                        "timer_id": event.id,
                        "due_at": event.due_at.to_rfc3339(),
                        "session": {
                            "id": event.session_id,
                            "platform": key.platform,
                            "scope": key.scope,
                            "scope_id": key.scope_id,
                            "user_id": key.user_id
                        },
                        "task": event.task
                    });
                    let input = crate::input_marker::wrap_timer(&input);
                    agent.emit(BackendEvent::MessageReceived {
                        session_id: session.id.clone(),
                        adapter_name: "timer".into(),
                        platform: session.session_key.platform.clone(),
                        user_id: session.session_key.user_id.clone(),
                        user_name: "timer".into(),
                        channel: if session.session_key.scope == "group" {
                            format!("group:{}", session.session_key.scope_id)
                        } else {
                            "direct".into()
                        },
                        group_name: session.group_name.clone(),
                        content: input.clone(),
                        images: vec![],
                        timestamp: received_at_ms / 1000,
                        received_at_ms,
                        message_sequence,
                    });
                    let branch_agent = Arc::clone(&agent);
                    let timer_id = event.id.clone();
                    tokio::spawn(async move {
                        match branch_agent.process_message(&session, &input).await {
                            Ok(content) if !content.trim().is_empty() => {
                                branch_agent.emit(BackendEvent::AgentOutput {
                                    session_id: session.id.clone(),
                                    content,
                                    branch_id: None,
                                });
                            }
                            Ok(_) => {}
                            Err(error) => {
                                tracing::warn!(%error, %timer_id, "timer task failed");
                                branch_agent.emit(BackendEvent::Error {
                                    session_id: Some(session.id.clone()),
                                    message: format!("timer {timer_id} failed: {error}"),
                                });
                            }
                        }
                    });
                }
            });
        }

        if let Some(mut receiver) = background_receiver {
            let agent = Arc::clone(self);
            let cancel = self.cancel.clone();
            tokio::spawn(async move {
                let mut ordered = orchestration::OrderedCompletionBuffer::new();
                loop {
                    let completion = tokio::select! {
                        completion = receiver.recv() => match completion {
                            Some(completion) => completion,
                            None => break,
                        },
                        _ = cancel.cancelled() => break,
                    };
                    let success = !completion.cancelled
                        && completion.branches.iter().all(|branch| branch.success);
                    agent.emit(BackendEvent::BackgroundTaskCompleted {
                        session_id: completion.session_id.clone(),
                        task_id: completion.task_id.clone(),
                        sequence: completion.sequence,
                        success,
                        completed_at_ms: completion.completed_at.timestamp_millis(),
                    });
                    for completion in ordered.push(completion) {
                        agent.integrate_background_completion(completion).await;
                    }
                }
            });
        }
    }

    async fn integrate_background_completion(
        &self,
        completion: orchestration::BackgroundCompletion,
    ) {
        let success =
            !completion.cancelled && completion.branches.iter().all(|branch| branch.success);
        let Some(key) = crate::session::SessionKey::parse(&completion.session_id) else {
            tracing::warn!(
                task_id = %completion.task_id,
                session = %completion.session_id,
                "background completion session is invalid"
            );
            return;
        };
        let session = self.trunk.get(&completion.session_id).unwrap_or_else(|| {
            self.trunk
                .get_or_create(&key, "background task".into(), None)
        });
        let received_at_ms = chrono::Utc::now().timestamp_millis();
        let message_sequence = self.next_message_sequence();
        let deliveries = completion
            .branches
            .iter()
            .map(|branch| serde_json::json!({
                "branch_id": branch.branch_id,
                "target": branch.target,
                "success": branch.success,
                "result": branch.result,
                "started_at": branch.started_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "completed_at": branch.completed_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            }))
            .collect::<Vec<_>>();
        let payload = serde_json::json!({
            "event": "background_task_completed",
            "message_sequence": message_sequence,
            "received_at_ms": received_at_ms,
            "task_id": completion.task_id,
            "task_sequence": completion.sequence,
            "objective": completion.objective,
            "created_at": completion.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "completed_at": completion.completed_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "cancelled": completion.cancelled,
            "origin_session": completion.session_id,
            "deliveries": deliveries
        });
        let input = crate::input_marker::wrap_background(&payload);
        self.emit(BackendEvent::MessageReceived {
            session_id: session.id.clone(),
            adapter_name: "background".into(),
            platform: key.platform,
            user_id: key.user_id,
            user_name: "background task".into(),
            channel: if key.scope == "group" {
                format!("group:{}", key.scope_id)
            } else {
                "direct".into()
            },
            group_name: session.group_name.clone(),
            content: input.clone(),
            images: vec![],
            timestamp: completion.completed_at.timestamp(),
            received_at_ms,
            message_sequence,
        });
        tracing::info!(
            task_id = %completion.task_id,
            task_sequence = completion.sequence,
            session = %session.id,
            created_at = %completion.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            completed_at = %completion.completed_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "integrating background task in creation order"
        );
        let integrated = match self.process_message(&session, &input).await {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(%error, task_id = %completion.task_id, "background integration failed");
                self.emit(BackendEvent::Error {
                    session_id: Some(session.id.clone()),
                    message: format!(
                        "background task {} integration failed: {error}",
                        completion.task_id
                    ),
                });
                false
            }
        };
        self.background_tasks
            .mark_integrated(&completion.task_id)
            .await;
        self.emit(BackendEvent::BackgroundTaskIntegrated {
            session_id: session.id,
            task_id: completion.task_id,
            sequence: completion.sequence,
            success: success && integrated,
            integrated_at_ms: chrono::Utc::now().timestamp_millis(),
        });
    }

    async fn reload_skills(&self, skills_dir: &str) -> Result<bool, String> {
        if !std::path::Path::new(skills_dir).exists() {
            return Ok(false);
        }

        let dir = skills_dir.to_string();
        let mut reloaded = tokio::task::spawn_blocking(move || SkillRegistry::discover(&dir))
            .await
            .map_err(|error| format!("skill reload task failed: {error}"))?
            .map_err(|error| error.to_string())?;

        let mut current = self.skills.lock().await;
        reloaded.inherit_enabled_from(&current);
        if *current == reloaded {
            return Ok(false);
        }

        let names = reloaded.names();
        *current = reloaded;
        drop(current);
        *self.system_prompt_cache.write().await = None;
        tracing::info!(skills = ?names, path = %skills_dir, "skills hot reloaded");
        Ok(true)
    }

    pub fn attach(&self, handle: Arc<BackendHandle>) {
        if let Ok(mut h) = self.handle.try_write() {
            *h = Some(handle);
        } else {
            tracing::warn!("handle slot busy, ignoring attach");
        }
    }

    /// Attach the sudo authorization broker (called once by the composition
    /// root). No-op when a broker is already attached.
    pub fn attach_sudo_broker(&self, broker: Arc<crate::sudo::SudoBroker>) {
        if let Ok(mut slot) = self.sudo_broker.try_write() {
            if slot.is_none() {
                *slot = Some(broker);
            } else {
                tracing::warn!("sudo broker slot busy, ignoring attach");
            }
        } else {
            tracing::warn!("sudo broker slot busy, ignoring attach");
        }
    }

    /// Non-blocking poll for a frontend command (used by the command pump).
    pub fn try_recv_command(&self) -> Option<BackendCommand> {
        self.handle
            .try_read()
            .ok()?
            .as_ref()
            .and_then(|h| h.try_recv_command())
    }

    /// Emit an event to subscribers (TUI, ...). No-op when unattached.
    ///
    /// The event is broadcast through the [`EventBus`] first (observe mode):
    /// the timeline projector and any other listeners consume it before the
    /// frontend hand-off, so the persisted display timeline and the wire both
    /// derive from the same emission.
    pub fn emit(&self, event: BackendEvent) {
        self.event_bus
            .emit_sync(event.clone(), echo_context::DispatchMode::Observe);
        if let Some(h) = self
            .handle
            .try_read()
            .ok()
            .as_ref()
            .and_then(|g| g.as_ref())
        {
            h.emit(event);
        }
    }

    fn emit_reasoning(&self, session_id: &str, branch_id: &str, reasoning: &Option<String>) {
        if let Some(content) = reasoning
            .as_deref()
            .filter(|content| !content.trim().is_empty())
        {
            self.emit(BackendEvent::AgentReasoning {
                session_id: session_id.to_string(),
                branch_id: branch_id.to_string(),
                content: content.to_string(),
            });
        }
    }

    pub(crate) fn next_message_sequence(&self) -> u64 {
        self.next_message_sequence.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn register_inbound_turn(
        &self,
        session_id: &str,
        message_sequence: u64,
        branch: bool,
    ) -> InboundTurnRegistration {
        let id = uuid::Uuid::new_v4().to_string();
        let cancel = tokio_util::sync::CancellationToken::new();
        self.active_inbound_turns.insert(
            id.clone(),
            ActiveInboundTurn {
                session_id: session_id.to_string(),
                message_sequence,
                branch,
                cancel: cancel.clone(),
            },
        );
        InboundTurnRegistration { id, cancel }
    }

    pub(crate) fn finish_inbound_turn(&self, turn_id: &str) {
        self.active_inbound_turns.remove(turn_id);
    }

    /// Cancel the newest active turn belonging to the command sender. Branches
    /// win ties so a follow-up cancellation targets the temporary work first.
    pub(crate) fn cancel_inbound_turns(&self, session_id: &str, all: bool) -> usize {
        let mut turns = self
            .active_inbound_turns
            .iter()
            .filter(|turn| turn.session_id == session_id)
            .map(|turn| (turn.message_sequence, turn.branch, turn.cancel.clone()))
            .collect::<Vec<_>>();
        turns.sort_by_key(|(sequence, branch, _)| (*sequence, *branch));
        if !all {
            turns = turns.into_iter().rev().take(1).collect();
        }
        let count = turns.len();
        for (_, _, cancel) in turns {
            cancel.cancel();
        }
        count
    }

    pub(crate) fn cancel_all_inbound_turns(&self) -> usize {
        let turns = self
            .active_inbound_turns
            .iter()
            .map(|turn| turn.cancel.clone())
            .collect::<Vec<_>>();
        let count = turns.len();
        for cancel in turns {
            cancel.cancel();
        }
        count
    }

    pub(crate) async fn cancel_requested_work(&self, session_id: &str, all: bool) -> usize {
        let foreground = self.cancel_inbound_turns(session_id, all);
        let background = if all || foreground == 0 {
            self.background_tasks
                .cancel_running_for_session(session_id, all)
                .await
        } else {
            0
        };
        foreground + background
    }

    /// Commit an inbound event and capture the branch's point-in-time context
    /// as one operation. The conversation lock is never held while the model
    /// or tools are running.
    pub(crate) async fn record_incoming_and_snapshot(
        &self,
        session: &Session,
        content: &str,
    ) -> Vec<ChatMessage> {
        let _turn = session.turn_lock.lock().await;
        self.trunk
            .append_event(echo_session::SessionEvent::UserMessage(
                echo_session::event::UserMessage {
                    content: content.to_string(),
                    timestamp: chrono::Utc::now().timestamp(),
                    message_sequence: structured_message_sequence(content),
                    source: None,
                    images: extract_input_images(content),
                },
            ));
        let snapshot = session.history.lock().await.clone();
        snapshot
    }

    /// Register a cancellable branch and record its input under the same
    /// conversation lock used by clear and merge operations.
    pub(crate) async fn register_incoming_branch(
        &self,
        session: &Session,
        content: &str,
        message_sequence: u64,
    ) -> (InboundTurnRegistration, Vec<ChatMessage>) {
        let _turn = session.turn_lock.lock().await;
        let registration = self.register_inbound_turn(&session.id, message_sequence, true);
        self.trunk
            .append_event(echo_session::SessionEvent::UserMessage(
                echo_session::event::UserMessage {
                    content: content.to_string(),
                    timestamp: chrono::Utc::now().timestamp(),
                    message_sequence: Some(message_sequence),
                    source: None,
                    images: extract_input_images(content),
                },
            ));
        let snapshot = session.history.lock().await.clone();
        (registration, snapshot)
    }

    async fn record_assistant_reply(
        &self,
        session: &Session,
        reply: String,
        message_sequence: Option<u64>,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> bool {
        if reply.trim().is_empty() {
            return true;
        }
        let _turn = session.turn_lock.lock().await;
        if cancel.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            return false;
        }
        // The assistant reply is a durable event; the event log re-derives
        // the trunk projection. When the reply answers a sequenced request
        // (concurrent branches), insert it after that request's user event so
        // the projected order matches request order.
        let event =
            echo_session::SessionEvent::AssistantMessage(echo_session::event::AssistantMessage {
                content: reply,
                reasoning_content: None,
                tool_calls: vec![],
            });
        match message_sequence {
            Some(sequence) => self.trunk.insert_event_after_sequence(sequence, event),
            None => self.trunk.append_event(event),
        }
        true
    }

    pub(crate) async fn record_control_reply(
        &self,
        session: &Session,
        reply: String,
        message_sequence: u64,
    ) {
        self.record_assistant_reply(session, reply, Some(message_sequence), None)
            .await;
    }

    pub(crate) async fn process_recorded_message(
        &self,
        session: &Session,
        content: &str,
        history_snapshot: Vec<ChatMessage>,
        cancel: tokio_util::sync::CancellationToken,
        branch_id: &str,
    ) -> Result<String> {
        self.process_recorded_message_with_progress(
            session,
            content,
            history_snapshot,
            cancel,
            None,
            branch_id,
        )
        .await
    }

    pub(crate) async fn process_recorded_message_with_progress(
        &self,
        session: &Session,
        content: &str,
        history_snapshot: Vec<ChatMessage>,
        cancel: tokio_util::sync::CancellationToken,
        visible_reply: Option<tokio::sync::watch::Sender<bool>>,
        branch_id: &str,
    ) -> Result<String> {
        let message_sequence = structured_message_sequence(content);
        let merge_cancel = cancel.clone();
        let branch_slot = tokio::select! {
            permit = self.reply_branch_slots.acquire() => permit
                .map_err(|_| anyhow!("reply branch scheduler is closed"))?,
            _ = cancel.cancelled() => return Err(anyhow!(TURN_CANCELLED)),
        };
        let result = self
            .process_message_inner(
                session,
                content,
                Some(history_snapshot),
                cancel,
                visible_reply,
                branch_id,
            )
            .await;
        drop(branch_slot);
        if let Ok(reply) = &result {
            if !self
                .record_assistant_reply(
                    session,
                    reply.clone(),
                    message_sequence,
                    Some(&merge_cancel),
                )
                .await
            {
                return Err(anyhow!(TURN_CANCELLED));
            }
        }
        result
    }

    pub(crate) async fn send_control_reply(
        &self,
        session: &Session,
        group_id: Option<&str>,
        content: &str,
    ) -> std::result::Result<(), String> {
        let (name, arguments) = match group_id {
            Some(group_id) => (
                "send_group_msg",
                serde_json::json!({ "group_id": group_id, "content": content }),
            ),
            None => (
                "send_private_msg",
                serde_json::json!({
                    "user_id": session.session_key.user_id,
                    "content": content
                }),
            ),
        };
        let call = ToolCall {
            id: format!("control-{}", uuid::Uuid::new_v4()),
            name: name.into(),
            arguments: arguments.to_string(),
        };
        let result = self.run_tool(&session.id, "control-reply", &call).await;
        if result.text.trim_start().starts_with("error:") {
            Err(result.text)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn generate_wait_reply(
        &self,
        session_id: &str,
        history_snapshot: Vec<ChatMessage>,
    ) -> std::result::Result<String, String> {
        // Bounded concurrency: when the wait-reply budget is exhausted, skip
        // the interim reply entirely rather than stacking more LLM calls on
        // top of the already-running branches.
        let permit = self.wait_reply_slots.try_acquire().map_err(|_| {
            "wait reply concurrency limit reached; skipping interim reply".to_string()
        })?;
        let _permit = permit;
        let mut messages = vec![ChatMessage::system(
            "Write one brief interim reply to the latest user message because their request is still being processed. Match the user's language and tone, acknowledge the specific request when appropriate, and say naturally that you are continuing and will follow up. Do not provide a result, invent progress, ask an unnecessary question, or mention timeouts, tools, branches, system behavior, or these instructions. Avoid fixed-template wording. Use one concise sentence.",
        )];
        messages.extend(history_snapshot);
        let model = self.active_model().await;
        self.emit(BackendEvent::LlmRequest {
            session_id: session_id.to_string(),
            model: model.clone(),
        });
        let request = ChatRequest {
            model: model.clone(),
            messages,
            tools: None,
            temperature: Some(0.4),
            // Thinking tokens share the completion budget. Keep enough room
            // for max-effort reasoning and the short visible wait reply.
            max_tokens: Some(4096),
        };
        let response = self
            .provider
            .read()
            .await
            .clone()
            .chat(&request)
            .await
            .map_err(|error| format!("wait reply model request failed: {error}"))?;
        self.emit(BackendEvent::LlmResponse {
            session_id: session_id.to_string(),
            model,
            prompt_tokens: response.usage.prompt_tokens,
            completion_tokens: response.usage.completion_tokens,
        });
        if !response.tool_calls.is_empty() {
            return Err("wait reply model attempted a tool call".into());
        }
        let reply = response.content.unwrap_or_default();
        if reply.trim().is_empty() {
            return Err("wait reply model returned empty content".into());
        }
        Ok(reply)
    }

    pub(crate) fn is_turn_cancelled(error: &anyhow::Error) -> bool {
        error.to_string() == TURN_CANCELLED
    }

    #[cfg(test)]
    pub(crate) fn active_inbound_turn_count(&self) -> usize {
        self.active_inbound_turns.len()
    }

    #[cfg(test)]
    pub(crate) fn active_inbound_branch_count(&self) -> usize {
        self.active_inbound_turns
            .iter()
            .filter(|turn| turn.branch)
            .count()
    }

    pub async fn active_model(&self) -> String {
        let model = self.active_model.read().await.clone();
        if model.is_empty() {
            self.provider.read().await.default_model().to_string()
        } else {
            model
        }
    }

    pub async fn active_provider(&self) -> String {
        self.provider.read().await.name().to_string()
    }

    pub async fn set_model(&self, model: String) {
        *self.active_model.write().await = model;
    }

    /// Process one user message through the agent loop and return the reply.
    pub async fn process_message(&self, session: &Session, content: &str) -> Result<String> {
        let message_sequence =
            structured_message_sequence(content).unwrap_or_else(|| self.next_message_sequence());
        let (registration, history_snapshot) = self
            .register_incoming_branch(session, content, message_sequence)
            .await;
        let branch_id = registration.id.clone();
        self.emit(BackendEvent::ReplyBranchStarted {
            session_id: session.id.clone(),
            branch_id: branch_id.clone(),
            message_sequence,
            task: content.to_string(),
            target: "源会话".into(),
            started_at_ms: chrono::Utc::now().timestamp_millis(),
        });
        let result = self
            .process_recorded_message(
                session,
                content,
                history_snapshot,
                registration.cancel.clone(),
                &branch_id,
            )
            .await;
        let cancelled = registration.cancel.is_cancelled()
            || result.as_ref().err().is_some_and(Self::is_turn_cancelled);
        self.finish_inbound_turn(&branch_id);
        self.emit(BackendEvent::ReplyBranchCompleted {
            session_id: session.id.clone(),
            branch_id,
            message_sequence,
            success: result.is_ok() && !cancelled,
            cancelled,
            completed_at_ms: chrono::Utc::now().timestamp_millis(),
        });
        result
    }

    /// Full lifecycle for an inbound platform message. This is the single
    /// implementation of the "register branch → emit lifecycle events →
    /// bounded wait reply → execute → merge → surface outcome" flow; every
    /// adapter (QQ, ...) routes through it so the paths cannot drift apart.
    ///
    /// Runs the branch in the background and returns immediately, matching the
    /// one-way inbound hook contract.
    pub(crate) async fn process_inbound_branch(
        self: &Arc<Self>,
        session: &Session,
        content: &str,
        message_sequence: u64,
        group_id: Option<String>,
        wait_reply_after: std::time::Duration,
    ) {
        let content = content.to_string();
        let (registration, history_snapshot) = self
            .register_incoming_branch(session, &content, message_sequence)
            .await;
        let branch_id = registration.id.clone();
        let session_id = session.id.clone();
        tracing::info!(
            session = %session_id,
            message_sequence,
            branch_id = %branch_id,
            started_at_ms = chrono::Utc::now().timestamp_millis(),
            "temporary inbound reply branch started"
        );
        self.emit(BackendEvent::ReplyBranchStarted {
            session_id: session_id.clone(),
            branch_id: branch_id.clone(),
            message_sequence,
            task: content.clone(),
            target: "源会话".into(),
            started_at_ms: chrono::Utc::now().timestamp_millis(),
        });

        let (visible_reply_tx, visible_reply_rx) = tokio::sync::watch::channel(false);
        let branch_completed = tokio_util::sync::CancellationToken::new();
        let group_id_for_wait = group_id.clone();
        spawn_contextual_wait_reply(
            Arc::clone(self),
            session.clone(),
            branch_id.clone(),
            group_id_for_wait,
            history_snapshot.clone(),
            visible_reply_rx,
            branch_completed.clone(),
            wait_reply_after,
        );

        let agent = Arc::clone(self);
        let session = session.clone();
        let session_id = session_id.clone();
        let branch_id = branch_id.clone();
        let group_id = group_id.clone();
        tokio::spawn(async move {
            let turn_cancel = registration.cancel.clone();
            let result = agent
                .process_recorded_message_with_progress(
                    &session,
                    &content,
                    history_snapshot,
                    turn_cancel.clone(),
                    Some(visible_reply_tx),
                    &branch_id,
                )
                .await;
            branch_completed.cancel();
            let cancelled = turn_cancel.is_cancelled()
                || result.as_ref().err().is_some_and(Self::is_turn_cancelled);
            agent.finish_inbound_turn(&branch_id);

            tracing::info!(
                session = %session_id,
                message_sequence,
                branch_id = %branch_id,
                success = result.is_ok() && !cancelled,
                cancelled,
                completed_at_ms = chrono::Utc::now().timestamp_millis(),
                "temporary inbound reply branch finished"
            );
            agent.emit(BackendEvent::ReplyBranchCompleted {
                session_id: session_id.clone(),
                branch_id: branch_id.clone(),
                message_sequence,
                success: result.is_ok() && !cancelled,
                cancelled,
                completed_at_ms: chrono::Utc::now().timestamp_millis(),
            });
            match result {
                Ok(output) => {
                    if !output.trim().is_empty() {
                        agent.emit(BackendEvent::AgentOutput {
                            session_id: session_id.clone(),
                            content: output,
                            branch_id: Some(branch_id.clone()),
                        });
                    }
                    tracing::info!(
                        session = %session_id,
                        message_sequence,
                        "inbound adapter message processed"
                    );
                }
                Err(error) if Self::is_turn_cancelled(&error) => {
                    agent.emit(BackendEvent::AgentCompleted {
                        session_id: session_id.clone(),
                    });
                    tracing::info!(
                        session = %session_id,
                        message_sequence,
                        "inbound adapter task cancelled"
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, session = %session_id, "agent processing failed");
                    agent.emit(BackendEvent::Error {
                        session_id: Some(session_id.clone()),
                        message: error.to_string(),
                    });
                    // The platform user has no view of backend Error events —
                    // send a bounded failure notice so they are not left
                    // wondering why no reply arrived.
                    let agent = Arc::clone(&agent);
                    let session = session.clone();
                    let session_id = session_id.clone();
                    let group_id = group_id.clone();
                    tokio::spawn(async move {
                        let notice = format!("处理失败：{error}");
                        let notice: String = notice.chars().take(500).collect();
                        if let Err(send_error) = agent
                            .send_control_reply(&session, group_id.as_deref(), &notice)
                            .await
                        {
                            tracing::warn!(%send_error, session = %session_id, "failure notice could not be delivered");
                        }
                    });
                }
            }
        });
    }

    async fn process_message_inner(
        &self,
        session: &Session,
        content: &str,
        history_snapshot: Option<Vec<ChatMessage>>,
        turn_cancel: tokio_util::sync::CancellationToken,
        visible_reply: Option<tokio::sync::watch::Sender<bool>>,
        branch_id: &str,
    ) -> Result<String> {
        let queued_at = std::time::Instant::now();
        let turn_started_at = chrono::Utc::now();
        let turn_id = uuid::Uuid::new_v4().to_string();
        let message_sequence = structured_message_sequence(content);
        let session_id = session.id.clone();
        tracing::info!(
            turn_id = %turn_id,
            message_sequence = message_sequence.unwrap_or_default(),
            session = %session_id,
            started_at = %turn_started_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            queue_wait_ms = queued_at.elapsed().as_millis() as u64,
            "agent turn started"
        );
        self.emit(BackendEvent::AgentThinking {
            session_id: session_id.clone(),
        });

        let delivery_plan = DeliveryPlan::from_input(content);
        // Distinguish input origin by content markers, NOT by the session's
        // platform. A QQ session can receive backend/TUI input (no hook), and
        // that must be answered in the backend — never pushed to QQ.
        let is_qq_hook = content.contains("<qq_message_hook>");
        let is_timer = content.contains(crate::input_marker::TIMER_EVENT_OPEN);
        let is_qq_session = session.session_key.platform.eq_ignore_ascii_case("qq");
        let boundary = if is_qq_hook {
            Some(BoundaryKind::QqHook)
        } else if is_timer {
            Some(BoundaryKind::Timer)
        } else if is_qq_session {
            Some(BoundaryKind::BackendInput)
        } else {
            None
        };
        // Build the system prompt as named blocks so the panel can visualize
        // token usage per section; keep the last build for `/context`.
        let blocks = self.build_prompt_blocks(content, boundary).await;
        let system_prompt = join_prompt_blocks(&blocks);
        *self.last_prompt_blocks.lock().await = Some(blocks);

        let mut messages = vec![ChatMessage::system(system_prompt)];
        if let Some(history_snapshot) = history_snapshot {
            messages.extend(history_snapshot);
        } else {
            let history = session.history.lock().await;
            messages.extend(history.iter().cloned());
        }

        let mut tools = (*self.tools.definitions().await).clone();
        let config = self.config.read().await;
        let self_update_enabled = config.self_update.enabled;
        let sudo_enabled = config.sudo.enabled;
        drop(config);
        tools.extend(orchestration::tool_definitions(
            self_update_enabled,
            sudo_enabled,
        ));
        // max_tool_iterations == 0 still allows one direct reply (without tools).
        let max_iterations = self.config.read().await.max_tool_iterations.max(1);
        let mut delivered_targets = std::collections::HashSet::new();
        let mut delivery_reminders = 0usize;

        for _ in 0..max_iterations {
            if turn_cancel.is_cancelled() {
                return Err(anyhow!(TURN_CANCELLED));
            }
            let model = self.active_model().await;
            self.emit(BackendEvent::LlmRequest {
                session_id: session_id.clone(),
                model: model.clone(),
            });
            let request = ChatRequest {
                model: model.clone(),
                messages: messages.clone(),
                tools: Some(tools.clone()),
                temperature: None,
                max_tokens: None,
            };
            let provider = self.provider.read().await.clone();
            let response = tokio::select! {
                response = provider.chat(&request) => response.map_err(|error| {
                    tracing::warn!(
                        turn_id = %turn_id,
                        message_sequence = message_sequence.unwrap_or_default(),
                        session = %session_id,
                        %error,
                        elapsed_ms = queued_at.elapsed().as_millis() as u64,
                        "agent turn model request failed"
                    );
                    error
                })?,
                _ = turn_cancel.cancelled() => return Err(anyhow!(TURN_CANCELLED)),
            };
            self.emit_reasoning(&session_id, branch_id, &response.reasoning_content);
            self.emit(BackendEvent::LlmResponse {
                session_id: session_id.clone(),
                model,
                prompt_tokens: response.usage.prompt_tokens,
                completion_tokens: response.usage.completion_tokens,
            });

            if response.tool_calls.is_empty() {
                let reply = response.content.unwrap_or_default();
                let pending_deliveries = delivery_plan
                    .as_ref()
                    .map(|plan| plan.pending(&delivered_targets))
                    .unwrap_or_default();
                if !pending_deliveries.is_empty() {
                    if delivery_reminders >= 2 {
                        return Err(anyhow!(
                            "required deliveries were not completed after two corrections: {}",
                            pending_deliveries
                                .iter()
                                .map(|target| echo_chat_capability::target_key(target))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    if !reply.trim().is_empty() {
                        messages.push(ChatMessage::assistant_with_reasoning(
                            reply,
                            response.reasoning_content.clone(),
                        ));
                    }
                    messages.push(ChatMessage::user(
                        QqDeliveryPolicy.delivery_reminder(&pending_deliveries),
                    ));
                    delivery_reminders += 1;
                    continue;
                }
                self.emit(BackendEvent::AgentCompleted {
                    session_id: session_id.clone(),
                });
                tracing::info!(
                    turn_id = %turn_id,
                    message_sequence = message_sequence.unwrap_or_default(),
                    session = %session_id,
                    completed_at = %chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    elapsed_ms = queued_at.elapsed().as_millis() as u64,
                    "agent turn completed"
                );
                return Ok(reply);
            }

            // LLM wants tools — execute them and continue the loop.
            messages.push(assistant_with_tool_calls(
                &response.tool_calls,
                &response.content,
                &response.reasoning_content,
            ));
            for call in &response.tool_calls {
                let delivery_key = match QqDeliveryPolicy.validate_delivery_call(
                    delivery_plan
                        .as_ref()
                        .map(|plan| plan.targets.as_slice())
                        .unwrap_or(&[]),
                    &mut delivered_targets,
                    call,
                ) {
                    Ok(delivery_key) => delivery_key,
                    Err(error) => {
                        let result = format!("error: {error}");
                        self.emit(BackendEvent::ToolCall {
                            session_id: session_id.clone(),
                            tool_name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            branch_id: branch_id.to_string(),
                        });
                        self.emit(BackendEvent::ToolResult {
                            session_id: session_id.clone(),
                            tool_name: call.name.clone(),
                            result: result.clone(),
                            branch_id: branch_id.to_string(),
                        });
                        messages.push(ChatMessage::tool(result, &call.id));
                        continue;
                    }
                };
                let tool_started = std::time::Instant::now();
                tracing::info!(
                    turn_id = %turn_id,
                    message_sequence = message_sequence.unwrap_or_default(),
                    session = %session_id,
                    tool_call_id = %call.id,
                    tool = %call.name,
                    "agent tool call started"
                );
                // Guard every tool with a timeout so a hung tool (e.g. an
                // unresponsive HTTP call) cannot stall the branch forever.
                let tool_timeout = self.config.read().await.effective_tool_timeout();
                let result = tokio::select! {
                    result = self.run_tool(&session_id, branch_id, call) => result,
                    _ = tokio::time::sleep(tool_timeout) => {
                        let text = format!("error: tool '{}' timed out after {}s", call.name, tool_timeout.as_secs());
                        // run_tool was dropped mid-flight: it already recorded
                        // the ToolCall event, so record the matching ToolResult
                        // or the durable log keeps a dangling call.
                        self.record_interrupted_tool_result(&session_id, branch_id, call, &text);
                        crate::tool::ToolResult::text(text)
                    }
                    _ = turn_cancel.cancelled() => {
                        self.record_interrupted_tool_result(
                            &session_id,
                            branch_id,
                            call,
                            "error: tool execution cancelled",
                        );
                        return Err(anyhow!(TURN_CANCELLED));
                    }
                };
                tracing::info!(
                    turn_id = %turn_id,
                    message_sequence = message_sequence.unwrap_or_default(),
                    session = %session_id,
                    tool_call_id = %call.id,
                    tool = %call.name,
                    success = !result.text.trim_start().starts_with("error:"),
                    elapsed_ms = tool_started.elapsed().as_millis() as u64,
                    "agent tool call completed"
                );
                if let Some(delivery_key) =
                    delivery_key.filter(|_| !result.text.trim_start().starts_with("error:"))
                {
                    delivered_targets.insert(delivery_key);
                    if let Some(visible_reply) = &visible_reply {
                        let _ = visible_reply.send(true);
                    }
                }
                messages.push(ChatMessage::tool_with_images(
                    result.text.clone(),
                    &call.id,
                    result.images.clone(),
                ));
            }
        }

        tracing::warn!(
            turn_id = %turn_id,
            message_sequence = message_sequence.unwrap_or_default(),
            session = %session_id,
            elapsed_ms = queued_at.elapsed().as_millis() as u64,
            max_iterations,
            "agent turn exhausted tool iterations"
        );
        Err(anyhow!(
            "reached max tool iterations ({}) without a final reply",
            max_iterations
        ))
    }

    /// 模型可见的工具参数预检：schema 必需字段缺失时返回纠正性错误文案
    /// （说清"你发了什么、应该发什么"），返回 None 表示通过预检。
    async fn tool_arguments_error(
        &self,
        tool_name: &str,
        raw_arguments: &str,
        args: &serde_json::Value,
    ) -> Option<String> {
        let schema = self.tools.parameters(tool_name).await?;
        invalid_tool_arguments(tool_name, raw_arguments, args, &schema)
    }

    async fn run_tool(
        &self,
        session_id: &str,
        branch_id: &str,
        call: &ToolCall,
    ) -> crate::tool::ToolResult {
        self.emit(BackendEvent::ToolCall {
            session_id: session_id.to_string(),
            tool_name: call.name.clone(),
            arguments: call.arguments.clone(),
            branch_id: branch_id.to_string(),
        });
        // The tool call is a durable event: the model-visible loop (call +
        // result) must be reconstructable from the log after a reload.
        self.trunk
            .append_event(echo_session::SessionEvent::ToolCall(
                echo_session::event::ToolCallEvent {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
            ));
        // 参数解析与预检：给模型可纠正的错误反馈。非法 JSON 或缺少必需
        // 字段时，错误文案必须说清"你发了什么、应该发什么"——一句模糊的
        // "command required" 只会让模型原样重试，形成空调用退化循环。
        let result = match serde_json::from_str::<serde_json::Value>(&call.arguments) {
            Err(error) => crate::tool::ToolResult::text(format!(
                "error: 工具参数不是合法 JSON（{error}）。你发送的原始参数: {}。请修正为合法 JSON 后重新调用 {}。",
                crate::llm::truncate(&call.arguments, 200),
                call.name,
            )),
            Ok(args) => match call.name.as_str() {
            "schedule_timer" => self
                .timer_scheduler
                .schedule(session_id, args)
                .await
                .map(crate::tool::ToolResult::text),
            "list_timers" => Ok(crate::tool::ToolResult::text(
                self.timer_scheduler.list(session_id).await,
            )),
            "cancel_timer" => self
                .timer_scheduler
                .cancel(session_id, args)
                .await
                .map(crate::tool::ToolResult::text),
            "run_subagent" => self
                .run_subagent(session_id, branch_id, args)
                .await
                .map(crate::tool::ToolResult::text),
            "spawn_background_task" => self
                .spawn_background_task(session_id, args, false)
                .await
                .map(crate::tool::ToolResult::text),
            "spawn_parallel_task" => self
                .spawn_background_task(session_id, args, true)
                .await
                .map(crate::tool::ToolResult::text),
            "list_background_tasks" => Ok(crate::tool::ToolResult::text(
                self.background_tasks.list(session_id).await,
            )),
            "cancel_background_task" => self
                .background_tasks
                .cancel(session_id, args)
                .await
                .map(crate::tool::ToolResult::text),
            "send_backend_message" => self
                .send_backend_message(args)
                .map(crate::tool::ToolResult::text),
            "framework_update" => {
                let config = self.config.read().await.self_update.clone();
                orchestration::framework_update(&config, session_id, args)
                    .await
                    .map(crate::tool::ToolResult::text)
            }
            "run_sudo" => self
                .run_sudo(session_id, args)
                .await
                .map(crate::tool::ToolResult::text),
            other => {
                // Every orchestration tool must live in the single dispatch
                // table; a name here that is not in the table would silently
                // bypass schema generation (drift between schema and handler).
                debug_assert!(
                    !crate::agent::orchestration::ORCHESTRATION_TOOL_NAMES.contains(&other),
                    "orchestration tool {other} missing from ORCHESTRATION_TOOL_NAMES"
                );
                match self.tool_arguments_error(other, &call.arguments, &args).await {
                    Some(message) => Err(message),
                    None => self
                        .tools
                        .execute_rich(other, args)
                        .await
                        .map_err(|error| error.to_string()),
                }
            }
        }
        .unwrap_or_else(|error| crate::tool::ToolResult::text(format!("error: {error}"))),
        };
        let result_text = result.text.clone();
        let result_images = result.images.clone();
        self.emit(BackendEvent::ToolResult {
            session_id: session_id.to_string(),
            tool_name: call.name.clone(),
            result: result_text.clone(),
            branch_id: branch_id.to_string(),
        });
        self.trunk
            .append_event(echo_session::SessionEvent::ToolResult(
                echo_session::event::ToolResultEvent {
                    tool_call_id: call.id.clone(),
                    result: result_text.clone(),
                    images: result_images.clone(),
                },
            ));
        if call.name == "checklist" {
            if let Some(state) = self.tools.snapshot(&call.name) {
                self.emit(BackendEvent::ChecklistUpdated {
                    session_id: session_id.to_string(),
                    state,
                });
            }
        }
        result
    }

    /// Record the `ToolResult` for a call whose `run_tool` future was dropped
    /// (timeout or cancellation). `run_tool` appends the `ToolCall` event
    /// before dispatching, so without this the durable log would keep a
    /// dangling call: after a reload the projection synthesizes an assistant
    /// tool_use no result answers, and the provider rejects the request
    /// (HTTP 400). Also emits the backend event so the UI can close out the
    /// pending tool row.
    fn record_interrupted_tool_result(
        &self,
        session_id: &str,
        branch_id: &str,
        call: &ToolCall,
        result: &str,
    ) {
        self.emit(BackendEvent::ToolResult {
            session_id: session_id.to_string(),
            tool_name: call.name.clone(),
            result: result.to_string(),
            branch_id: branch_id.to_string(),
        });
        self.trunk
            .append_event(echo_session::SessionEvent::ToolResult(
                echo_session::event::ToolResultEvent {
                    tool_call_id: call.id.clone(),
                    result: result.to_string(),
                    images: vec![],
                },
            ));
    }

    /// Run a command with root privileges (the `run_sudo` orchestration tool).
    ///
    /// The password is never seen by the LLM: a pending request is registered
    /// with the [`SudoBroker`](crate::sudo::SudoBroker), a `SudoRequest` event
    /// tells the Panel to prompt the user, and the Panel answers on the
    /// dedicated sudo channel (bypassing the agent command queue and the
    /// session log). The command's stdout/stderr are returned; the password
    /// itself never enters the context.
    async fn run_sudo(&self, session_id: &str, args: serde_json::Value) -> Result<String, String> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| "run_sudo: `command` (string) is required".to_string())?
            .to_string();
        if command.trim().is_empty() {
            return Err("run_sudo: command must not be empty".into());
        }

        let config = self.config.read().await.sudo.clone();
        if !config.enabled {
            return Err("run_sudo: sudo is disabled (set [agent.sudo] enabled = true)".into());
        }
        let broker = self
            .sudo_broker
            .read()
            .await
            .clone()
            .ok_or_else(|| "run_sudo: sudo broker not attached".to_string())?;

        let pending = broker.request();
        let request_id = pending.request_id;
        self.emit(BackendEvent::SudoRequest {
            request_id,
            command: command.clone(),
            session_id: session_id.to_string(),
        });

        let receiver = pending.into_receiver();
        let password = tokio::time::timeout(
            std::time::Duration::from_secs(config.auth_timeout_secs.max(1)),
            receiver,
        )
        .await
        .map_err(|_| {
            broker.cancel(request_id);
            self.emit(BackendEvent::SudoResolved {
                request_id,
                accepted: false,
                message: "sudo 授权超时".into(),
            });
            format!(
                "sudo authorization timed out after {}s — no password was submitted",
                config.auth_timeout_secs
            )
        })?
        .map_err(|_| {
            broker.cancel(request_id);
            self.emit(BackendEvent::SudoResolved {
                request_id,
                accepted: false,
                message: "sudo 授权通道关闭".into(),
            });
            "sudo authorization channel closed".to_string()
        })?
        .ok_or_else(|| {
            broker.cancel(request_id);
            self.emit(BackendEvent::SudoResolved {
                request_id,
                accepted: false,
                message: "sudo 授权被拒绝".into(),
            });
            "sudo authorization denied by the user".to_string()
        })?;

        self.emit(BackendEvent::SudoResolved {
            request_id,
            accepted: true,
            message: "sudo 已授权，正在执行".into(),
        });
        crate::sudo::run_sudo_command(
            &command,
            &password,
            std::time::Duration::from_secs(config.command_timeout_secs.max(1)),
        )
        .await
    }

    async fn spawn_background_task(
        &self,
        session_id: &str,
        arguments: serde_json::Value,
        parallel: bool,
    ) -> Result<String, String> {
        let work = if parallel {
            orchestration::parse_parallel_task_args(arguments, session_id)?
        } else {
            orchestration::parse_background_task_args(arguments, session_id)?
        };
        let objective = work.objective.clone();
        let branch_count = work.branches.len();
        let history_snapshot = match self.trunk.get(session_id) {
            Some(session) => session.history.lock().await.clone(),
            None => return Err("current session no longer exists".into()),
        };
        let runtime = orchestration::BackgroundRuntime {
            provider: self.provider.read().await.clone(),
            tools: Arc::clone(&self.tools),
            model: self.active_model().await,
            max_tool_iterations: self.config.read().await.max_tool_iterations,
            tool_timeout: self.config.read().await.effective_tool_timeout(),
            history_snapshot,
        };
        let receipt = self
            .background_tasks
            .spawn(session_id, work, runtime)
            .await?;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&receipt) {
            self.emit(BackendEvent::BackgroundTaskStarted {
                session_id: session_id.to_string(),
                task_id: value["task_id"].as_str().unwrap_or("unknown").to_string(),
                sequence: value["sequence"].as_u64().unwrap_or_default(),
                branch_count,
                objective,
                created_at_ms: chrono::DateTime::parse_from_rfc3339(
                    value["created_at"].as_str().unwrap_or(""),
                )
                .map(|time| time.timestamp_millis())
                .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis()),
            });
        }
        Ok(receipt)
    }

    fn send_backend_message(&self, arguments: serde_json::Value) -> Result<String, String> {
        let session_id = arguments["session_id"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "send_backend_message requires session_id".to_string())?;
        let content = arguments["content"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "send_backend_message requires non-empty content".to_string())?;
        self.emit(BackendEvent::AgentOutput {
            session_id: session_id.to_string(),
            content: content.to_string(),
            branch_id: None,
        });
        Ok(serde_json::json!({
            "status": "delivered",
            "session_id": session_id,
            "delivered_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        })
        .to_string())
    }

    async fn run_subagent(
        &self,
        session_id: &str,
        branch_id: &str,
        arguments: serde_json::Value,
    ) -> Result<String, String> {
        let args = orchestration::parse_subagent_args(arguments)?;
        self.emit(BackendEvent::SubagentStarted {
            session_id: session_id.to_string(),
            task: args.task.clone(),
        });
        let mut messages = vec![ChatMessage::system(
            "You are an isolated subagent. Complete the bounded task and return a concise, factual result to the parent agent. You have no tools and must not claim to perform external actions or communicate with users.",
        )];
        if let Some(context) = args.context {
            messages.push(ChatMessage::user(format!("Context:\n{context}")));
        }
        messages.push(ChatMessage::user(format!("Task:\n{}", args.task)));
        let request = ChatRequest {
            model: self.active_model().await,
            messages,
            tools: None,
            temperature: None,
            max_tokens: None,
        };
        let provider = self.provider.read().await.clone();
        let response = match provider.chat(&request).await {
            Ok(response) => response,
            Err(error) => {
                self.emit(BackendEvent::SubagentCompleted {
                    session_id: session_id.to_string(),
                    success: false,
                });
                return Err(format!("subagent request failed: {error}"));
            }
        };
        self.emit_reasoning(session_id, branch_id, &response.reasoning_content);
        if !response.tool_calls.is_empty() {
            self.emit(BackendEvent::SubagentCompleted {
                session_id: session_id.to_string(),
                success: false,
            });
            return Err("subagent attempted a tool call, but subagents have no tools".into());
        }
        let content = response.content.unwrap_or_default();
        if content.trim().is_empty() {
            self.emit(BackendEvent::SubagentCompleted {
                session_id: session_id.to_string(),
                success: false,
            });
            return Err("subagent returned an empty result".into());
        }
        self.emit(BackendEvent::SubagentCompleted {
            session_id: session_id.to_string(),
            success: true,
        });
        Ok(content)
    }

    /// System prompt = base prompt + skill metadata + triggered skill instructions.
    /// System prompt = base prompt + skill metadata + triggered skill
    /// instructions, decomposed into named blocks for panel visualization.
    async fn build_prompt_blocks(
        &self,
        content: &str,
        boundary: Option<BoundaryKind>,
    ) -> Vec<PromptBlock> {
        let skills = self.skills.lock().await;
        let matched = skills.find_matching(content);
        let base = self.config.read().await.system_prompt.clone();
        let mut blocks = Vec::new();
        blocks.push(PromptBlock {
            key: "base".into(),
            label: "系统提示词".into(),
            kind: "base".into(),
            content: base,
        });
        if !skills.is_empty() {
            blocks.push(PromptBlock {
                key: "skills".into(),
                label: "技能清单".into(),
                kind: "skills".into(),
                content: format!("# Available skills\n{}", skills.metadata_lines()),
            });
        }
        for skill in skills.always_enabled() {
            blocks.push(PromptBlock {
                key: format!("skill:{}", skill.metadata.name),
                label: format!("常驻技能 · {}", skill.metadata.name),
                kind: "skill".into(),
                content: format!(
                    "# Active skill: {}\n{}",
                    skill.metadata.name, skill.instructions
                ),
            });
        }
        if let Some(matched) = matched {
            blocks.push(PromptBlock {
                key: format!("triggered:{}", matched.metadata.name),
                label: format!("触发技能 · {}", matched.metadata.name),
                kind: "triggered".into(),
                content: format!(
                    "# Triggered skill: {}\n{}",
                    matched.metadata.name, matched.instructions
                ),
            });
        }
        blocks.push(PromptBlock {
            key: "orchestration".into(),
            label: "后台编排".into(),
            kind: "orchestration".into(),
            content: "# Background orchestration\n\
             Complete the current request normally; do not spawn detached work \
             merely to keep the conversation responsive. Use run_subagent for \
             bounded delegated reasoning. Use spawn_background_task only when \
             the requester explicitly asks for detached work, and \
             spawn_parallel_task only for independent work with declared \
             delivery targets. After detached work is accepted, do not poll it; \
             completion returns as an ordered background_task_event. Use \
             list_background_tasks only when status is requested and \
             cancel_background_task only on an explicit cancellation."
                .into(),
        });
        if let Some(boundary) = boundary {
            blocks.push(boundary.block());
        }
        blocks
    }

    /// Decomposed context blocks for `/context`: the prompt sections of the
    /// most recent turn (falling back to a boundary-free build) plus the
    /// conversation history aggregated per role. The trunk history holds user
    /// / assistant / tool messages only — the system prompt is rebuilt every
    /// turn and is represented by the prompt blocks above.
    pub async fn context_blocks(
        &self,
        history: &[ChatMessage],
    ) -> Vec<crate::event::ContextBlockInfo> {
        let prompt = match self.last_prompt_blocks.lock().await.clone() {
            Some(blocks) => blocks,
            // No turn has run yet in this process — build a representative
            // set without input-specific sections.
            None => self.build_prompt_blocks("", None).await,
        };
        let mut blocks: Vec<crate::event::ContextBlockInfo> = prompt
            .iter()
            .map(|block| crate::event::ContextBlockInfo {
                key: block.key.clone(),
                label: block.label.clone(),
                kind: block.kind.clone(),
                tokens: crate::llm::estimate_message_tokens(&ChatMessage::system(
                    block.content.clone(),
                )),
                chars: block.content.chars().count(),
                content: block.content.clone(),
            })
            .collect();
        // Aggregate the history per role.
        let mut by_role: std::collections::BTreeMap<&str, Vec<&ChatMessage>> =
            std::collections::BTreeMap::new();
        for message in history {
            by_role
                .entry(match message.role {
                    crate::llm::ChatRole::System => "system",
                    crate::llm::ChatRole::User => "user",
                    crate::llm::ChatRole::Assistant => "assistant",
                    crate::llm::ChatRole::Tool => "tool",
                })
                .or_default()
                .push(message);
        }
        for (role, messages) in by_role {
            let tokens: usize = messages
                .iter()
                .map(|message| crate::llm::estimate_message_tokens(message))
                .sum();
            let content = messages
                .iter()
                .map(|message| {
                    let preview = message
                        .content
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let preview: String = preview.chars().take(120).collect();
                    format!(
                        "[{}] {}t {}",
                        role,
                        crate::llm::estimate_message_tokens(message),
                        preview
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            blocks.push(crate::event::ContextBlockInfo {
                key: format!("history:{role}"),
                label: format!("对话历史 · {}", role_label(role)),
                kind: "history".into(),
                tokens,
                chars: content.chars().count(),
                content,
            });
        }
        blocks
    }

    /// System prompt string (kept for tests; the turn loop uses blocks).
    #[allow(dead_code)]
    async fn build_system_prompt(&self, content: &str) -> String {
        let skills = self.skills.lock().await;
        let matched = skills.find_matching(content);
        // No skill triggered — use cache if available.
        if matched.is_none() {
            if let Some(ref cached) = *self.system_prompt_cache.read().await {
                return cached.clone();
            }
        }
        let base = self.config.read().await.system_prompt.clone();
        let mut prompt = base;
        if !skills.is_empty() {
            prompt.push_str("\n\n# Available skills\n");
            prompt.push_str(&skills.metadata_lines());
        }
        for skill in skills.always_enabled() {
            prompt.push_str(&format!(
                "\n\n# Active skill: {}\n{}",
                skill.metadata.name, skill.instructions
            ));
        }
        if let Some(matched) = matched {
            prompt.push_str(&format!(
                "\n\n# Triggered skill: {}\n{}",
                matched.metadata.name, matched.instructions
            ));
            return prompt; // don't cache triggered-skill prompts
        }
        // Cache the base+metadata prompt.
        let mut cache = self.system_prompt_cache.write().await;
        *cache = Some(prompt.clone());
        prompt
    }

    /// Emit the current adapter list to subscribers.
    fn emit_adapter_list(&self) {
        let adapters: Vec<AdapterStatus> = self
            .adapters
            .list_info()
            .into_iter()
            .map(|info| AdapterStatus {
                name: info.name,
                display_name: info.display_name,
                connected: matches!(info.status, echo_adapter::AdapterConnectionState::Connected),
                running: !matches!(info.status, echo_adapter::AdapterConnectionState::Stopped),
                self_id: info.self_id,
                bind_address: String::new(), // populated by QqAdapter
                started_at: info.started_at,
            })
            .collect();
        self.emit(BackendEvent::AdapterList { adapters });
    }

    /// Current API configuration summary (for the TUI settings form).
    pub async fn api_config(&self) -> AgentConfig {
        self.config.read().await.clone()
    }

    /// Emit the current API configuration to subscribers.
    pub async fn emit_api_config(&self) {
        let cfg = self.api_config().await;
        let mut resolved = cfg.clone();
        resolved.apply_active_profile();
        let key_set = !resolved.effective_api_key().is_empty();
        let profiles: Vec<crate::event::ApiProfileInfo> = cfg
            .api_profiles
            .iter()
            .map(|p| crate::event::ApiProfileInfo {
                name: p.name.clone(),
                provider: p.provider.clone(),
                model: p.model.clone(),
                base_url: p.base_url.clone(),
                api_key_set: !p.api_key.is_empty(),
                thinking: p.thinking,
                reasoning_effort: p.reasoning_effort,
            })
            .collect();
        self.emit(BackendEvent::ApiConfigUpdated {
            provider: resolved.provider.clone(),
            model: self.active_model().await,
            base_url: resolved.effective_base_url(),
            api_key_set: key_set,
            thinking: resolved.thinking,
            reasoning_effort: resolved.reasoning_effort,
            system_prompt: cfg.system_prompt.clone(),
            active_api: cfg.active_api.clone(),
            profiles: profiles.clone(),
        });
        self.emit(BackendEvent::ApiProfilesUpdated {
            active_api: cfg.active_api.clone(),
            profiles,
        });
    }

    /// Rebuild the LLM provider from the current config snapshot.
    async fn rebuild_provider(&self, cfg: &AgentConfig) -> bool {
        let mut resolved = cfg.clone();
        resolved.apply_active_profile();
        resolved.api_key = resolved.effective_api_key();
        resolved.base_url = resolved.effective_base_url();
        match crate::llm::create_provider(&resolved) {
            Ok(p) => {
                *self.provider.write().await = Arc::from(p);
                true
            }
            Err(e) => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("failed to build provider: {e}"),
                });
                false
            }
        }
    }

    /// Save an API profile (or the top-level default when `name` is empty),
    /// activate it, rebuild the provider, and persist.
    #[allow(clippy::too_many_arguments)]
    async fn update_api_config(
        &self,
        name: String,
        provider: String,
        model: String,
        base_url: String,
        api_key: String,
        thinking: Option<crate::config::ThinkingMode>,
        reasoning_effort: Option<crate::config::ReasoningEffort>,
    ) {
        let mut config = self.config.write().await;
        let keep_key = api_key.is_empty();
        // 新建命名 profile 时，空的 provider/model/base_url 从当前生效配置
        // 继承，避免写出残缺 profile：残缺 profile 单独做 TestApi 时必然
        // "provider build failed"，且极易误导用户以为 LLM 整体不可用。
        // （更新已有 profile 时保持"空 = 保留该 profile 原值"的语义。）
        let (provider, model, base_url) =
            if !name.is_empty() && !config.api_profiles.iter().any(|p| p.name == name) {
                let mut resolved = config.clone();
                resolved.apply_active_profile();
                (
                    if provider.is_empty() {
                        resolved.provider
                    } else {
                        provider
                    },
                    if model.is_empty() {
                        resolved.model
                    } else {
                        model
                    },
                    if base_url.is_empty() {
                        resolved.base_url
                    } else {
                        base_url
                    },
                )
            } else {
                (provider, model, base_url)
            };
        if name.is_empty() {
            if !provider.is_empty() {
                config.provider = provider.clone();
            }
            if !model.is_empty() {
                config.model = model.clone();
            }
            if !base_url.is_empty() {
                config.base_url = base_url.clone();
            }
            if !keep_key {
                config.api_key = api_key.clone();
            }
            if let Some(thinking) = thinking {
                config.thinking = thinking;
            }
            if let Some(reasoning_effort) = reasoning_effort {
                config.reasoning_effort = reasoning_effort;
            }
            // Don't clear active_api — let explicit SwitchApi handle that.
        } else {
            let profile = match config.api_profiles.iter_mut().find(|p| p.name == name) {
                Some(p) => p,
                None => {
                    config.api_profiles.push(ApiProfile::new(
                        name.clone(),
                        provider.clone(),
                        model.clone(),
                    ));
                    let idx = config.api_profiles.len() - 1;
                    &mut config.api_profiles[idx]
                }
            };
            if !provider.is_empty() {
                profile.provider = provider.clone();
            }
            if !model.is_empty() {
                profile.model = model.clone();
            }
            if !base_url.is_empty() {
                profile.base_url = base_url.clone();
            }
            if !keep_key {
                profile.api_key = api_key.clone();
            }
            if let Some(thinking) = thinking {
                profile.thinking = thinking;
            }
            if let Some(reasoning_effort) = reasoning_effort {
                profile.reasoning_effort = reasoning_effort;
            }
            config.active_api = name.clone();
        }
        let cfg_snapshot = config.clone();
        drop(config);

        self.set_model(model.clone()).await;
        if self.rebuild_provider(&cfg_snapshot).await {
            self.emit(BackendEvent::Error {
                session_id: None,
                message: if name.is_empty() {
                    format!("default API updated: {provider} / {model}")
                } else {
                    format!("API saved and activated: {name} ({provider} / {model})")
                },
            });
        }
        self.emit_api_config().await;
        self.persist_config(&cfg_snapshot).await;
    }

    /// Switch to an API profile (or the top-level default when name is empty).
    async fn switch_api(&self, name: &str) {
        let mut config = self.config.write().await;
        if name.is_empty() {
            config.active_api.clear();
        } else {
            if !config.api_profiles.iter().any(|p| p.name == name) {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("API config not found: {name}"),
                });
                return;
            }
            config.active_api = name.to_string();
        }
        let cfg_snapshot = config.clone();
        let mut resolved = cfg_snapshot.clone();
        resolved.apply_active_profile();
        let model = resolved.model.clone();
        drop(config);

        if self.rebuild_provider(&cfg_snapshot).await {
            self.set_model(model).await;
            let label = if name.is_empty() {
                "default config".to_string()
            } else {
                name.to_string()
            };
            self.emit(BackendEvent::Error {
                session_id: None,
                message: format!("switched to API: {label}"),
            });
        }
        self.emit_api_config().await;
        self.persist_config(&cfg_snapshot).await;
    }

    /// Delete an API profile; if it was active, fall back to the top-level default.
    async fn delete_api(&self, name: &str) {
        let mut config = self.config.write().await;
        let before = config.api_profiles.len();
        config.api_profiles.retain(|p| p.name != name);
        let removed = config.api_profiles.len() < before;
        if removed && config.active_api == name {
            config.active_api.clear();
        }
        let cfg_snapshot = config.clone();
        drop(config);

        if removed {
            if self.rebuild_provider(&cfg_snapshot).await {
                self.set_model(cfg_snapshot.model.clone()).await;
            }
            self.emit(BackendEvent::Error {
                session_id: None,
                message: format!("deleted API config: {name}"),
            });
            self.emit_api_config().await;
            self.persist_config(&cfg_snapshot).await;
        } else {
            self.emit(BackendEvent::Error {
                session_id: None,
                message: format!("API config not found: {name}"),
            });
        }
    }

    /// Test connectivity of an API config by sending a minimal probe request.
    ///
    /// `name` empty tests the active (top-level) config; otherwise the named
    /// profile's values are used. Emits `BackendEvent::ApiTestResult`; the
    /// active provider is never modified.
    async fn test_api_config(&self, name: &str) {
        let timestamp_start = std::time::Instant::now();
        let config = self.config.read().await.clone();

        // Resolve the config to test: named profile or the active default.
        let probe = match resolve_probe_config(&config, name) {
            Ok(probe) => probe,
            Err(message) => {
                self.emit(BackendEvent::ApiTestResult {
                    name: name.into(),
                    ok: false,
                    message,
                    latency_ms: timestamp_start.elapsed().as_millis() as u64,
                });
                return;
            }
        };

        // Build a throwaway provider from the probe config.
        let provider = match create_provider(&probe) {
            Ok(provider) => provider,
            Err(error) => {
                self.emit(BackendEvent::ApiTestResult {
                    name: name.into(),
                    ok: false,
                    message: format!("provider build failed: {error}"),
                    latency_ms: timestamp_start.elapsed().as_millis() as u64,
                });
                return;
            }
        };

        let model = probe.model.clone();
        let request = ChatRequest {
            model: model.clone(),
            messages: vec![ChatMessage::user("ping")],
            tools: None,
            temperature: Some(0.0),
            max_tokens: Some(4),
        };
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(20), provider.chat(&request)).await;

        let latency_ms = timestamp_start.elapsed().as_millis() as u64;
        let (ok, message) = match result {
            Ok(Ok(response)) => (
                true,
                match response.content {
                    Some(content) if !content.trim().is_empty() => format!(
                        "OK ({}) — 响应: {}",
                        model,
                        echo_defs::token::truncate(content.trim(), 60)
                    ),
                    _ => format!("OK ({model}) — 收到空响应"),
                },
            ),
            Ok(Err(error)) => (false, format!("请求失败: {error}")),
            Err(_) => (false, "请求超时（>20s）".into()),
        };
        self.emit(BackendEvent::ApiTestResult {
            name: name.into(),
            ok,
            message,
            latency_ms,
        });
    }

    /// Persist the system prompt plugin text to `[plugins.system_prompt]`
    /// instead of `[agent]`. The API config no longer owns the system prompt.
    async fn persist_system_prompt_plugin(&self, text: &str) {
        let store = match self.config_store.lock().await.clone() {
            Some(s) => s,
            None => return,
        };
        let text = text.to_string();
        if let Err(error) = store.patch(|root| {
            let plugins = echo_adapter::ensure_table(root, "plugins");
            let system_prompt = echo_adapter::ensure_table(plugins, "system_prompt");
            system_prompt.insert("text".into(), toml::Value::String(text));
            Ok(())
        }) {
            tracing::warn!(error = %error, "failed to persist system prompt plugin");
        }
    }

    /// Persist a config snapshot's `[agent]` section back to the TOML file
    /// through the shared [`echo_adapter::ConfigStore`].
    async fn persist_config(&self, cfg: &AgentConfig) {
        let store = match self.config_store.lock().await.clone() {
            Some(s) => s,
            None => return,
        };
        let mut value = match toml::Value::try_from(cfg) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize agent config");
                return;
            }
        };
        tracing::debug!(api_key_len = cfg.api_key.len(), "persisting agent config");
        if let Err(e) = store.patch(|root| {
            // `api_key` is excluded from AgentConfig's generic serialization.
            // Write the explicit in-memory key when set; otherwise keep the
            // on-disk value so unrelated persists never strip it from the file.
            let key_value = if cfg.api_key.is_empty() {
                root.get("agent")
                    .and_then(|agent| agent.get("api_key"))
                    .cloned()
            } else {
                Some(toml::Value::String(cfg.api_key.clone()))
            };
            if let Some(key_value) = key_value {
                value
                    .as_table_mut()
                    .expect("serialized agent config is a table")
                    .insert("api_key".into(), key_value);
            }
            root.insert("agent".into(), value.clone());
            Ok(())
        }) {
            tracing::warn!(error = %e, "failed to persist agent config");
            return;
        }
        tracing::info!(path = %store.path().display(), "agent config persisted");
    }
}

/// Build the config snapshot used for a connectivity probe.
///
/// `name` empty → active (top-level) config with the active profile merged
/// in; otherwise the named profile's non-empty values override the current
/// effective config. Returns an error string when the profile does not exist
/// or the resolved config has no provider.
fn resolve_probe_config(
    config: &crate::config::AgentConfig,
    name: &str,
) -> Result<crate::config::AgentConfig, String> {
    let mut probe = config.clone();
    if name.is_empty() {
        probe.apply_active_profile();
    } else {
        let profile = config
            .api_profiles
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("profile not found: {name}"))?;
        // 与 apply_active_profile 一致的宽松语义：profile 的空字段
        // 回退到当前生效值，避免字段残缺的 profile 测不出本来的配置。
        if !profile.provider.is_empty() {
            probe.provider = profile.provider.clone();
        }
        if !profile.model.is_empty() {
            probe.model = profile.model.clone();
        }
        if !profile.base_url.is_empty() {
            probe.base_url = profile.base_url.clone();
        }
        if !profile.api_key.is_empty() {
            probe.api_key = profile.api_key.clone();
        }
        probe.thinking = profile.thinking;
        probe.reasoning_effort = profile.reasoning_effort;
    }
    probe.api_key = probe.effective_api_key();
    probe.base_url = probe.effective_base_url();
    if probe.provider.is_empty() {
        return Err(format!(
            "provider not set (name={name}); 请先在设置中配置 Provider/API 或激活某个 profile"
        ));
    }
    Ok(probe)
}

#[derive(Debug, Clone)]
/// The QQ delivery policy: parses deliveries from QQ/background inputs and
/// validates send tool calls against the declared targets. This is the
/// platform implementation of the [`DeliveryPolicy`] seam; the loop depends
/// only on the trait.
struct QqDeliveryPolicy;

struct DeliveryPlan {
    targets: Vec<DeliveryTarget>,
}

impl DeliveryPlan {
    fn from_input(content: &str) -> Option<Self> {
        QqDeliveryPolicy
            .plan_from_input(content)
            .map(|targets| Self { targets })
    }

    fn pending<'a>(
        &'a self,
        delivered: &std::collections::HashSet<String>,
    ) -> Vec<&'a DeliveryTarget> {
        self.targets
            .iter()
            .filter(|target| !delivered.contains(&echo_chat_capability::target_key(target)))
            .collect()
    }
}

/// The QQ implementation of the delivery policy seam: parses QQ hooks and
/// background-task deliveries, and validates `send_*` tool calls against the
/// declared targets. The loop never imports QQ tool names directly — it
/// drives deliveries through [`DeliveryPolicy`].
impl DeliveryPolicy for QqDeliveryPolicy {
    fn plan_from_input(&self, content: &str) -> Option<Vec<DeliveryTarget>> {
        if let Some(target) = Self::from_qq_hook(content) {
            return Some(vec![target]);
        }
        let payload = content
            .trim()
            .strip_prefix(crate::input_marker::BACKGROUND_EVENT_OPEN)?
            .strip_suffix("</background_task_event>")?
            .trim();
        let value: serde_json::Value = serde_json::from_str(payload).ok()?;
        let mut targets = Vec::new();
        for delivery in value["deliveries"].as_array()? {
            let target = &delivery["target"];
            let parsed = match target["kind"].as_str()? {
                "backend" => DeliveryTarget::Backend {
                    session_id: json_id(&target["session_id"])?,
                },
                "qq_private" => DeliveryTarget::Direct {
                    user_id: json_id(&target["user_id"])?,
                },
                "qq_group" => DeliveryTarget::Group {
                    group_id: json_id(&target["group_id"])?,
                },
                _ => return None,
            };
            if !targets.contains(&parsed) {
                targets.push(parsed);
            }
        }
        (!targets.is_empty()).then_some(targets)
    }

    fn validate_delivery_call(
        &self,
        plan: &[DeliveryTarget],
        delivered: &mut std::collections::HashSet<String>,
        call: &ToolCall,
    ) -> Result<Option<String>, String> {
        if !matches!(
            call.name.as_str(),
            "send_private_msg" | "send_group_msg" | "send_backend_message"
        ) {
            return Ok(None);
        }
        if plan.is_empty() {
            return Ok(None);
        }
        let args: serde_json::Value = serde_json::from_str(&call.arguments)
            .map_err(|error| format!("invalid delivery arguments: {error}"))?;
        let target = plan
            .iter()
            .find(|target| {
                if call.name != target_tool_name(target) {
                    return false;
                }
                let (id_name, expected) = target_expected_id(target);
                json_id(&args[id_name]).as_deref() == Some(expected)
            })
            .ok_or_else(|| format!("delivery target is not declared for {}", call.name))?;
        let key = echo_chat_capability::target_key(target);
        // 允许同一目标多次投递：回复条数完全由 agent 决定（例如先回
        // 一条图片说明、再回一条文字）。`delivered` 集合仅用于判定"是否
        // 已满足至少一次投递"（驱动 delivery reminder），不再拦截重复。
        delivered.insert(key.clone());
        Ok(Some(key))
    }

    fn delivery_reminder(&self, pending: &[&DeliveryTarget]) -> String {
        let required = pending
            .iter()
            .map(|target| {
                let (id_name, id) = target_expected_id(target);
                format!("{} with {id_name}={id}", target_tool_name(target))
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "<backend_delivery_correction>The previous response did not complete all declared deliveries. Call these tools now, exactly once per target: {required}. Use the corresponding branch result as content. Do not return another direct answer before every tool succeeds.</backend_delivery_correction>"
        )
    }
}

impl QqDeliveryPolicy {
    fn from_qq_hook(content: &str) -> Option<DeliveryTarget> {
        let payload = content
            .trim()
            .strip_prefix("<qq_message_hook>")?
            .strip_suffix("</qq_message_hook>")?
            .trim();
        let value: serde_json::Value = serde_json::from_str(payload).ok()?;
        match value.pointer("/channel/type")?.as_str()? {
            "private" => Some(DeliveryTarget::Direct {
                user_id: json_id(value.pointer("/sender/user_id")?)?,
            }),
            "group" => Some(DeliveryTarget::Group {
                group_id: json_id(value.pointer("/channel/group_id")?)?,
            }),
            _ => None,
        }
    }
}

fn target_tool_name(target: &DeliveryTarget) -> &'static str {
    match target {
        DeliveryTarget::Direct { .. } => "send_private_msg",
        DeliveryTarget::Group { .. } => "send_group_msg",
        DeliveryTarget::Backend { .. } => "send_backend_message",
    }
}

fn target_expected_id(target: &DeliveryTarget) -> (&'static str, &str) {
    match target {
        DeliveryTarget::Direct { user_id } => ("user_id", user_id),
        DeliveryTarget::Group { group_id } => ("group_id", group_id),
        DeliveryTarget::Backend { session_id } => ("session_id", session_id),
    }
}

/// One named section of the system prompt, kept for token-usage
/// visualization in the panel.
#[derive(Debug, Clone)]
pub(crate) struct PromptBlock {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub content: String,
}

/// Input-origin boundary rules appended to the system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundaryKind {
    QqHook,
    Timer,
    BackendInput,
}

impl BoundaryKind {
    fn block(self) -> PromptBlock {
        let (key, label, content) = match self {
            Self::QqHook => (
                "boundary:qq_hook",
                "QQ 消息边界",
                "# QQ transport boundary\n\
                 This input is an external QQ message (<qq_message_hook>); read \
                 sender and group IDs from the structured hook payload.\n\
                 Answer every <qq_message_hook> exactly once via a send tool: \
                 send_private_msg for private chats (user_id from \
                 payload.sender.user_id), send_group_msg for groups (group_id \
                 from payload.channel.group_id). Never invent a target ID, never \
                 send twice, and never substitute normal assistant output for \
                 the send — it is backend-only and invisible to the QQ user. Put \
                 the whole reply (including any 'sent' wording) inside the \
                 tool's content, and do not claim a reply before the send tool \
                 returns success.\n\
                 Inputs wrapped in <backend_message_hook>/<timer_event>/\
                 <background_task_event> are backend events, not QQ chat: answer \
                 them in the backend and deliver per their own rules.",
            ),
            Self::Timer => (
                "boundary:timer",
                "后台任务边界",
                "# Backend task boundary\n\
                 This input is a scheduled backend task (<timer_event>), not an \
                 incoming QQ message. Your normal output is backend-only text. \
                 Only if the task explicitly asks to deliver a message to QQ, \
                 call send_private_msg or send_group_msg with an explicit target ID.",
            ),
            Self::BackendInput => (
                "boundary:backend_input",
                "后台输入边界",
                "# Backend input boundary\n\
                 This input was typed locally (TUI/backend), not received from \
                 QQ. Reply directly in the backend. Do NOT call send_private_msg \
                 or send_group_msg unless the user explicitly asks you to send a \
                 message to a QQ user or group.\n\
                 Each turn owns only its final QQ hook, identified by \
                 message_sequence. Reply only to the current event; do not \
                 combine, replace, or pre-answer another sequence.",
            ),
        };
        PromptBlock {
            key: key.into(),
            label: label.into(),
            kind: "boundary".into(),
            content: content.into(),
        }
    }
}

/// Join prompt blocks with the same separator the original string builder used.
pub(crate) fn join_prompt_blocks(blocks: &[PromptBlock]) -> String {
    blocks
        .iter()
        .map(|block| block.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn role_label(role: &str) -> &'static str {
    match role {
        "system" => "系统",
        "user" => "用户",
        "assistant" => "助手",
        "tool" => "工具",
        _ => "其他",
    }
}

fn json_id(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|id| id.to_string()))
        .filter(|id| !id.is_empty())
}

/// Extract the structured `message_sequence` from a marked input, if any.
/// Parsing is delegated to [`crate::input_marker`] (single source of truth).
pub(crate) fn structured_message_sequence(content: &str) -> Option<u64> {
    crate::input_marker::structured_message_sequence(content)
}

/// 模型可见的工具参数预检：schema 声明的必需字段缺失时，返回纠正性错误
/// 文案（说清"你发了什么、应该发什么"）。返回 None 表示参数通过预检。
///
/// 设计动机：模型在长工具循环中可能退化出空参调用（如 run_command {}），
/// 若错误反馈只是模糊的"command required"，模型不知道错在哪，会原样
/// 重试形成退化循环，烧掉整段上下文预算。
pub(crate) fn invalid_tool_arguments(
    tool_name: &str,
    raw_arguments: &str,
    args: &serde_json::Value,
    schema: &serde_json::Value,
) -> Option<String> {
    let required: Vec<&str> = schema["required"]
        .as_array()?
        .iter()
        .filter_map(|field| field.as_str())
        .collect();
    if required.is_empty() {
        return None;
    }
    // 缺失 = 键不存在或值为 null（空字符串保留给各工具自己判定，
    // 避免把 write_file content:"" 这类合法调用误判为缺参）。
    let missing: Vec<&str> = match args.as_object() {
        Some(obj) => required
            .iter()
            .filter(|field| obj.get(**field).map_or(true, |v| v.is_null()))
            .copied()
            .collect(),
        None => required.clone(),
    };
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "工具参数无效: {tool_name} 缺少必需参数 {}（你发送的参数: {}）。\
         该工具的参数 schema: {}。请按 schema 携带全部必需参数重新调用。",
        missing.join(", "),
        crate::llm::truncate(raw_arguments, 200),
        schema,
    ))
}

/// Extract image URLs / data URIs from a structured hook input.
///
/// Hook payloads carry `"images": [...]` (set by
/// [`crate::adapter_bridge::format_hook_input`]); this pulls them into the
/// durable `UserMessage` event so session replay keeps the multimodal
/// content. Returns an empty vec for plain text.
/// 单张图片（URL 或 data URI 字符串）进入持久化/模型请求的上限。
/// 超过的 data URI 直接丢弃（downstream 已按源码大小限制，这里兜底，
/// 防止超大 base64 撑大会话日志与每次请求的 prompt）。
const MAX_INPUT_IMAGE_CHARS: usize = 8 * 1024 * 1024;

pub(crate) fn extract_input_images(content: &str) -> Vec<String> {
    let Some(payload) = crate::input_marker::hook_payload(content) else {
        return Vec::new();
    };
    let value: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    value["images"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    m.as_str()
                        .filter(|s| s.len() <= MAX_INPUT_IMAGE_CHARS)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}
/// After a delay, generate and send one interim reply for a still-running
/// branch, unless the branch already produced a visible reply or finished.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_contextual_wait_reply(
    agent: Arc<Agent>,
    session: crate::session::Session,
    branch_id: String,
    group_id: Option<String>,
    history_snapshot: Vec<crate::llm::ChatMessage>,
    mut visible_reply: tokio::sync::watch::Receiver<bool>,
    branch_completed: tokio_util::sync::CancellationToken,
    delay: std::time::Duration,
) {
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {
                if branch_completed.is_cancelled() || *visible_reply.borrow() {
                    return;
                }
                let generated = tokio::select! {
                    result = agent.generate_wait_reply(&session.id, history_snapshot) => result,
                    changed = visible_reply.changed() => {
                        let _ = changed;
                        return;
                    }
                    _ = branch_completed.cancelled() => return,
                };
                let reply = match generated {
                    Ok(reply) => reply,
                    Err(error) => {
                        tracing::warn!(%error, session = %session.id, "wait reply generation skipped");
                        return;
                    }
                };
                if branch_completed.is_cancelled() || *visible_reply.borrow() {
                    return;
                }
                if let Err(error) = agent
                    .send_control_reply(&session, group_id.as_deref(), &reply)
                    .await
                {
                    tracing::warn!(%error, session = %session.id, "wait reply send failed");
                    agent.emit(BackendEvent::ReplyBranchContent {
                        session_id: session.id.clone(),
                        branch_id,
                        content: format!("临时回复发送失败：{error}\n待发送内容：{reply}"),
                    });
                } else {
                    agent.emit(BackendEvent::ReplyBranchContent {
                        session_id: session.id.clone(),
                        branch_id,
                        content: reply,
                    });
                }
            }
            changed = visible_reply.changed() => {
                let _ = changed;
            }
            _ = branch_completed.cancelled() => {}
        }
    });
}

fn assistant_with_tool_calls(
    calls: &[ToolCall],
    content: &Option<String>,
    reasoning_content: &Option<String>,
) -> ChatMessage {
    ChatMessage {
        role: crate::llm::ChatRole::Assistant,
        content: content.clone().unwrap_or_default(),
        reasoning_content: reasoning_content.clone(),
        tool_calls: Some(calls.to_vec()),
        tool_call_id: None,
        images: vec![],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::llm::{ChatChunk, ChatResponse, LlmError, Usage};
    use crate::session::SessionKey;
    use crate::tool::{Tool, ToolError};
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub struct MockProvider {
        pub calls: Arc<AtomicUsize>,
        pub reply: String,
    }

    struct ConcurrentProvider {
        entered: Arc<AtomicUsize>,
        release: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }
        fn default_model(&self) -> &str {
            "mock-model"
        }
        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.messages[0].role, crate::llm::ChatRole::System);
            Ok(ChatResponse {
                content: Some(self.reply.clone()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            })
        }
        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<(), LlmError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ConcurrentProvider {
        fn name(&self) -> &str {
            "concurrent"
        }

        fn default_model(&self) -> &str {
            "concurrent"
        }

        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.release
                .acquire()
                .await
                .expect("release semaphore should stay open")
                .forget();
            let sequence = request
                .messages
                .iter()
                .filter_map(|message| structured_message_sequence(&message.content))
                .next_back()
                .unwrap_or_default();
            Ok(ChatResponse {
                content: Some(format!("reply-{sequence}")),
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: Usage::default(),
            })
        }

        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<(), LlmError> {
            Ok(())
        }
    }

    fn test_agent(provider: Arc<dyn LlmProvider>) -> Agent {
        Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            ToolRegistry::new(),
            Arc::new(AdapterRegistry::new()),
        )
    }

    #[tokio::test]
    async fn emitted_events_are_recorded_into_the_display_timeline() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "Hello!".into(),
        });
        let agent = test_agent(provider);
        let key = SessionKey::local_tui();
        let session = agent.trunk.get_or_create(&key, "user".into(), None);
        // Adapter flow: inbound message event, then branch execution, then output.
        agent.emit(BackendEvent::MessageReceived {
            session_id: session.id.clone(),
            adapter_name: "local".into(),
            platform: "local".into(),
            user_id: "local_user".into(),
            user_name: "local user".into(),
            channel: "direct".into(),
            group_name: None,
            content: "hi".into(),
            images: vec![],
            timestamp: 1700000000,
            received_at_ms: 1700000000123,
            message_sequence: 1,
        });
        let reply = agent.process_message(&session, "hi").await.unwrap();
        agent.emit(BackendEvent::AgentOutput {
            session_id: session.id.clone(),
            content: reply.clone(),
            branch_id: None,
        });
        assert_eq!(reply, "Hello!");

        let timeline = agent.trunk.timeline_snapshot();
        // user entry + backend reply entry.
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[0].kind, "user");
        assert_eq!(timeline[0].content, "hi");
        assert!(timeline[0].source.is_some(), "source provenance recorded");
        assert_eq!(timeline[1].kind, "backend");
        assert_eq!(timeline[1].content, "Hello!");
    }

    #[tokio::test]
    async fn timeline_records_tools_and_attaches_outcomes() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MockTool {
            name: "mock_tool",
            result: "found".into(),
        }));
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let call = ToolCall {
            id: "c1".into(),
            name: "mock_tool".into(),
            arguments: r#"{"query":"x","api_key":"secret-1"}"#.into(),
        };
        let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
        assert_eq!(result.text, "found");

        let timeline = agent.trunk.timeline_snapshot();
        assert_eq!(timeline.len(), 1);
        let tool = timeline[0].tool.as_ref().expect("tool entry");
        assert_eq!(tool.name, "mock_tool");
        assert!(tool.output.is_some(), "outcome attached");
        assert!(!tool.input.contains("secret-1"), "secrets redacted");
        assert!(tool.input.contains("[已隐藏]"), "redaction marker shown");
        assert!(!tool.failed);
    }

    #[tokio::test]
    async fn timeline_skips_background_hooks_and_timer_summaries_cleanly() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = test_agent(provider);
        // Background hook — must NOT appear in the timeline.
        agent.emit(BackendEvent::MessageReceived {
            session_id: "local:tui::one".into(),
            adapter_name: "background".into(),
            platform: "qq".into(),
            user_id: "1".into(),
            user_name: "background task".into(),
            channel: "direct".into(),
            group_name: None,
            content: "<background_task_event>{}</background_task_event>".into(),
            images: vec![],
            timestamp: 1,
            received_at_ms: 1000,
            message_sequence: 1,
        });
        assert!(agent.trunk.timeline_snapshot().is_empty());

        // Timer event — becomes a system summary.
        agent.emit(BackendEvent::MessageReceived {
            session_id: "local:tui::one".into(),
            adapter_name: "timer".into(),
            platform: "local".into(),
            user_id: "1".into(),
            user_name: "timer".into(),
            channel: "direct".into(),
            group_name: None,
            content: r#"<timer_event>{"message_sequence":2,"task":"发送提醒"}</timer_event>"#
                .into(),
            images: vec![],
            timestamp: 2,
            received_at_ms: 2000,
            message_sequence: 2,
        });
        let timeline = agent.trunk.timeline_snapshot();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].kind, "system");
        assert_eq!(timeline[0].content, "定时任务触发 · 发送提醒");
    }

    #[tokio::test]
    async fn timeline_associates_reasoning_with_the_final_output() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "done".into(),
        });
        let agent = test_agent(provider);
        let session_id = "local:tui::one";
        let branch_id = "branch-abc";
        agent.emit(BackendEvent::AgentReasoning {
            session_id: session_id.into(),
            branch_id: branch_id.into(),
            content: "先查资料".into(),
        });
        agent.emit(BackendEvent::ReplyBranchCompleted {
            session_id: session_id.into(),
            branch_id: branch_id.into(),
            message_sequence: 1,
            success: true,
            cancelled: false,
            completed_at_ms: 1000,
        });
        agent.emit(BackendEvent::AgentOutput {
            session_id: session_id.into(),
            content: "done".into(),
            branch_id: Some(branch_id.into()),
        });
        let timeline = agent.trunk.timeline_snapshot();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].kind, "backend");
        assert_eq!(
            timeline[0].reasoning.as_deref(),
            Some(vec!["先查资料".to_string()].as_slice()),
            "reasoning attached to its branch output"
        );
    }

    #[tokio::test]
    async fn agent_replies_and_remembers() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "Hello!".into(),
        });
        let agent = test_agent(provider.clone());
        let key = SessionKey::local_tui();
        let session = agent.trunk.get_or_create(&key, "user".into(), None);
        let reply = agent.process_message(&session, "hi").await.unwrap();
        assert_eq!(reply, "Hello!");
        assert_eq!(session.history.lock().await.len(), 2);
        let _ = agent.process_message(&session, "again").await.unwrap();
        assert_eq!(session.history.lock().await.len(), 4);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn ordinary_turns_run_in_parallel_and_merge_by_request_sequence() {
        let entered = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let provider = Arc::new(ConcurrentProvider {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let agent = Arc::new(test_agent(provider));
        let session = agent
            .trunk
            .get_or_create(&SessionKey::local_tui(), "user".into(), None);
        let first = r#"<backend_message_hook>{"message_sequence":1,"content":"first"}</backend_message_hook>"#;
        let second = r#"<backend_message_hook>{"message_sequence":2,"content":"second"}</backend_message_hook>"#;

        let first_task = {
            let agent = Arc::clone(&agent);
            let session = session.clone();
            tokio::spawn(async move { agent.process_message(&session, first).await })
        };
        let second_task = {
            let agent = Arc::clone(&agent);
            let session = session.clone();
            tokio::spawn(async move { agent.process_message(&session, second).await })
        };

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while entered.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both branches should enter the provider concurrently");
        release.add_permits(2);
        first_task.await.unwrap().unwrap();
        second_task.await.unwrap().unwrap();

        let history = session.history.lock().await;
        assert_eq!(history.len(), 4);
        assert_eq!(structured_message_sequence(&history[0].content), Some(1));
        assert_eq!(history[1].content, "reply-1");
        assert_eq!(structured_message_sequence(&history[2].content), Some(2));
        assert_eq!(history[3].content, "reply-2");
    }

    #[tokio::test]
    async fn request_trunk_timeline_returns_the_persisted_history() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Arc::new(test_agent(provider));
        let (bridge, handle) = crate::create_bridge();
        agent.attach(Arc::new(handle));
        agent.emit(BackendEvent::MessageReceived {
            session_id: "local:tui::local_user".into(),
            adapter_name: "local".into(),
            platform: "local".into(),
            user_id: "local_user".into(),
            user_name: "local user".into(),
            channel: "direct".into(),
            group_name: None,
            content: "你好".into(),
            images: vec![],
            timestamp: 1700000000,
            received_at_ms: 1700000000123,
            message_sequence: 1,
        });

        agent
            .apply_command(BackendCommand::RequestTrunkTimeline)
            .await;
        let mut events = Vec::new();
        while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
            events.push(event);
        }
        let timeline = events
            .into_iter()
            .find_map(|event| match event {
                BackendEvent::TrunkTimeline { messages } => Some(messages),
                _ => None,
            })
            .expect("TrunkTimeline event emitted");
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].kind, "user");
        assert_eq!(timeline[0].content, "你好");
    }

    #[tokio::test]
    async fn run_sudo_emits_request_and_resolves_with_password() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Arc::new(test_agent(provider));
        let (bridge, handle) = crate::create_bridge();
        agent.attach(Arc::new(handle));
        let broker = Arc::new(crate::sudo::SudoBroker::new());
        agent.attach_sudo_broker(broker.clone());
        // Enable sudo and use the fake-sudo-friendly `true` (no real root
        // needed; with a wrong password sudo still exits and returns text).
        {
            let mut config = agent.config.write().await;
            config.sudo.enabled = true;
            config.sudo.auth_timeout_secs = 30;
            config.sudo.command_timeout_secs = 30;
        }
        let session_id = "local:tui::one";
        agent
            .trunk
            .get_or_create(&SessionKey::parse(session_id).unwrap(), "user".into(), None);
        let call = ToolCall {
            id: "sudo-1".into(),
            name: "run_sudo".into(),
            arguments: r#"{"command":"true"}"#.into(),
        };
        let agent_for_task = agent.clone();
        let task =
            tokio::spawn(
                async move { agent_for_task.run_tool(session_id, "branch-1", &call).await },
            );

        // Wait for the SudoRequest event.
        let mut request_id = None;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let mut events = Vec::new();
            while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
                if let BackendEvent::SudoRequest { request_id: id, .. } = event {
                    request_id = Some(id);
                    break;
                }
                events.push(event);
            }
            if request_id.is_some() {
                break;
            }
        }
        let request_id = request_id.expect("SudoRequest event emitted");
        assert!(broker.submit(request_id, Some("hunter2".into())));

        let result = task.await.expect("tool completes");
        assert!(
            !result.text.contains("hunter2"),
            "password must never reach the model: {}",
            result.text
        );
        // The command itself ran (or failed with a sudo error) — never a leak.
        assert!(!result.text.is_empty());
    }

    #[tokio::test]
    async fn run_sudo_denied_returns_error() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Arc::new(test_agent(provider));
        let (bridge, handle) = crate::create_bridge();
        agent.attach(Arc::new(handle));
        let broker = Arc::new(crate::sudo::SudoBroker::new());
        agent.attach_sudo_broker(broker.clone());
        {
            let mut config = agent.config.write().await;
            config.sudo.enabled = true;
            config.sudo.auth_timeout_secs = 30;
        }
        let session_id = "local:tui::one";
        agent
            .trunk
            .get_or_create(&SessionKey::parse(session_id).unwrap(), "user".into(), None);
        let call = ToolCall {
            id: "sudo-2".into(),
            name: "run_sudo".into(),
            arguments: r#"{"command":"true"}"#.into(),
        };
        let agent_for_task = agent.clone();
        let task =
            tokio::spawn(
                async move { agent_for_task.run_tool(session_id, "branch-1", &call).await },
            );
        let mut request_id = None;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
                if let BackendEvent::SudoRequest { request_id: id, .. } = event {
                    request_id = Some(id);
                    break;
                }
            }
            if request_id.is_some() {
                break;
            }
        }
        let request_id = request_id.expect("SudoRequest event emitted");
        assert!(broker.submit(request_id, None));
        let result = task.await.expect("tool completes");
        assert!(
            result.text.contains("denied"),
            "denial must surface to the model: {}",
            result.text
        );
    }

    #[tokio::test]
    async fn run_sudo_disabled_returns_error_without_broker() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Arc::new(test_agent(provider));
        let session_id = "local:tui::one";
        agent
            .trunk
            .get_or_create(&SessionKey::parse(session_id).unwrap(), "user".into(), None);
        let call = ToolCall {
            id: "sudo-3".into(),
            name: "run_sudo".into(),
            arguments: r#"{"command":"true"}"#.into(),
        };
        let result = agent.run_tool(session_id, "branch-1", &call).await;
        assert!(
            result.text.contains("disabled"),
            "unexpected: {}",
            result.text
        );
    }

    #[tokio::test]
    async fn update_api_config_persists() {
        let tmp =
            std::env::temp_dir().join(format!("echo-agent-cfg-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(
            &tmp,
            "[server]\nx = 1\n\n[agent]\nprovider = \"openai\"\nmodel = \"old-model\"\n",
        )
        .unwrap();
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = test_agent(provider);
        agent.set_config_path(tmp.clone());

        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: String::new(),
                provider: "anthropic".into(),
                model: "claude-x".into(),
                base_url: "http://example.test".into(),
                api_key: "sk-test-123".into(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;

        let cfg = agent.api_config().await;
        assert_eq!(cfg.provider, "anthropic");
        assert_eq!(cfg.model, "claude-x");
        assert_eq!(cfg.base_url, "http://example.test");
        assert_eq!(cfg.effective_api_key(), "sk-test-123");
        assert!(cfg.active_api.is_empty());

        let content = std::fs::read_to_string(&tmp).unwrap();
        assert!(content.contains("[server]\nx = 1"));
        assert!(content.contains("provider = \"anthropic\""));
        assert!(content.contains("model = \"claude-x\""));
        assert!(content.contains("base_url = \"http://example.test\""));
        assert!(content.contains("api_key = \"sk-test-123\""));
        assert!(!content.contains("old-model"));
        std::fs::remove_file(&tmp).ok();
    }

    #[tokio::test]
    async fn persist_config_keeps_on_disk_api_key() {
        let tmp =
            std::env::temp_dir().join(format!("echo-agent-cfg-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(
            &tmp,
            "[agent]\nprovider = \"openai\"\napi_key = \"sk-on-disk\"\n",
        )
        .unwrap();
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = test_agent(provider);
        agent.set_config_path(tmp.clone());

        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: String::new(),
                provider: "anthropic".into(),
                model: "claude-x".into(),
                base_url: "http://example.test".into(),
                api_key: String::new(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;

        let content = std::fs::read_to_string(&tmp).unwrap();
        assert!(content.contains("provider = \"anthropic\""));
        assert!(
            content.contains("api_key = \"sk-on-disk\""),
            "unexpected: {content}"
        );
        std::fs::remove_file(&tmp).ok();
    }

    #[tokio::test]
    async fn update_api_config_new_profile_inherits_empty_fields() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = test_agent(provider);

        // 先建立顶层默认配置。
        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: String::new(),
                provider: "deepseek".into(),
                model: "deepseek-x".into(),
                base_url: "http://ds.test".into(),
                api_key: "sk-top".into(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;

        // 用全空字段创建命名 profile：应从当前生效配置继承，而不是写出
        // 残缺 profile（真实事故：TUI 只改 model 保存后 profile 缺字段）。
        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: "backup".into(),
                provider: String::new(),
                model: String::new(),
                base_url: String::new(),
                api_key: String::new(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;

        let cfg = agent.api_config().await;
        assert_eq!(cfg.active_api, "backup");
        let profile = cfg
            .api_profiles
            .iter()
            .find(|p| p.name == "backup")
            .expect("profile created");
        assert_eq!(profile.provider, "deepseek", "provider inherited");
        assert_eq!(profile.model, "deepseek-x", "model inherited");
        assert_eq!(profile.base_url, "http://ds.test", "base_url inherited");

        // 更新已有 profile 时保持"空 = 保留原值"语义。
        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: "backup".into(),
                provider: String::new(),
                model: "deepseek-y".into(),
                base_url: String::new(),
                api_key: String::new(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;
        let cfg = agent.api_config().await;
        let profile = cfg
            .api_profiles
            .iter()
            .find(|p| p.name == "backup")
            .expect("profile exists");
        assert_eq!(profile.provider, "deepseek", "provider kept, not inherited");
        assert_eq!(profile.model, "deepseek-y");
    }

    #[tokio::test]
    async fn test_api_config_profile_empty_fields_fall_back() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Arc::new(test_agent(provider));
        let (bridge, handle) = crate::create_bridge();
        agent.attach(Arc::new(handle));

        // 顶层配置指向一个必然连不上的地址（连接即刻被拒绝，不打真实网络）。
        agent
            .apply_command(BackendCommand::UpdateApiConfig {
                name: String::new(),
                provider: "deepseek".into(),
                model: "deepseek-x".into(),
                base_url: "http://127.0.0.1:1".into(),
                api_key: "sk-top".into(),
                thinking: None,
                reasoning_effort: None,
            })
            .await;

        // 手工注入一个字段残缺的 profile（provider/base_url 为空）。
        agent.config.write().await.api_profiles.push(ApiProfile {
            name: "broken".into(),
            provider: String::new(),
            model: "deepseek-x".into(),
            base_url: String::new(),
            api_key: String::new(),
            thinking: crate::config::ThinkingMode::Enabled,
            reasoning_effort: crate::config::ReasoningEffort::Max,
        });

        agent
            .apply_command(BackendCommand::TestApi {
                name: "broken".into(),
            })
            .await;

        let mut result = None;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            while let Ok(event) = bridge.event_rx.lock().await.try_recv() {
                if let BackendEvent::ApiTestResult { name, message, .. } = event {
                    result = Some((name, message));
                    break;
                }
            }
            if result.is_some() {
                break;
            }
        }
        let (name, message) = result.expect("ApiTestResult event emitted");
        assert_eq!(name, "broken");
        assert!(
            !message.contains("provider build failed"),
            "空字段应回退到顶层配置而不是构建失败: {message}"
        );
        // 回退后 provider 构建成功，失败只可能发生在请求阶段（连接拒绝）。
        assert!(
            message.contains("请求失败") || message.contains("请求超时"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn invalid_tool_arguments_flags_missing_required() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
        });
        // 空对象：缺 command
        let message = invalid_tool_arguments("run_command", "{}", &serde_json::json!({}), &schema)
            .expect("missing required flagged");
        assert!(message.contains("缺少必需参数 command"), "{message}");
        assert!(message.contains("你发送的参数: {}"), "{message}");
        assert!(message.contains("schema"), "{message}");
        // null 值同样算缺失
        assert!(invalid_tool_arguments(
            "run_command",
            "{}",
            &serde_json::json!({"command": null}),
            &schema
        )
        .is_some());
        // 参数不是对象：所有必需字段都缺失
        assert!(
            invalid_tool_arguments("run_command", "[]", &serde_json::Value::Null, &schema)
                .is_some()
        );
        // 字段齐全（含空字符串——合法值，不算缺失）：通过
        assert!(invalid_tool_arguments(
            "run_command",
            "{}",
            &serde_json::json!({"command": ""}),
            &schema,
        )
        .is_none());
        // schema 无 required：不预检
        let free = serde_json::json!({"type": "object", "properties": {}});
        assert!(invalid_tool_arguments("stub", "{}", &serde_json::json!({}), &free).is_none());
    }

    #[tokio::test]
    async fn run_tool_empty_arguments_gets_corrective_error() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let mut tools = ToolRegistry::new();
        crate::tool::builtin::coding::register_coding_tools(&mut tools, std::env::temp_dir());
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let call = ToolCall {
            id: "bad-1".into(),
            name: "run_command".into(),
            arguments: "{}".into(),
        };
        let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
        assert!(
            result.text.contains("缺少必需参数 command"),
            "corrective message: {}",
            result.text
        );
        assert!(
            result.text.contains("你发送的参数: {}"),
            "echoes received args: {}",
            result.text
        );
        assert!(
            result.text.contains("schema"),
            "includes tool schema: {}",
            result.text
        );
    }

    #[tokio::test]
    async fn run_tool_malformed_json_gets_clear_error() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let mut tools = ToolRegistry::new();
        crate::tool::builtin::coding::register_coding_tools(&mut tools, std::env::temp_dir());
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let call = ToolCall {
            id: "bad-2".into(),
            name: "run_command".into(),
            arguments: "{bad json".into(),
        };
        let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
        assert!(
            result.text.contains("工具参数不是合法 JSON"),
            "clear JSON error: {}",
            result.text
        );
        assert!(
            result.text.contains("{bad json"),
            "echoes raw arguments: {}",
            result.text
        );
    }

    #[tokio::test]
    async fn run_tool_valid_arguments_pass_preflight() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let mut tools = ToolRegistry::new();
        crate::tool::builtin::coding::register_coding_tools(&mut tools, std::env::temp_dir());
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let call = ToolCall {
            id: "ok-1".into(),
            name: "run_command".into(),
            arguments: r#"{"command":"echo preflight-ok"}"#.into(),
        };
        let result = agent.run_tool("local:tui::one", "branch-1", &call).await;
        assert!(
            result.text.contains("preflight-ok"),
            "valid call executes: {}",
            result.text
        );
        assert!(
            !result.text.contains("工具参数无效"),
            "no preflight error: {}",
            result.text
        );
    }

    #[tokio::test]
    async fn context_blocks_decompose_prompt_and_history_by_role() {
        let mut reg = SkillRegistry::new();
        reg.register(crate::skill::Skill {
            metadata: crate::skill::SkillMetadata {
                name: "calc".into(),
                description: "d".into(),
                keywords: vec!["calc".into()],
                always: false,
                enabled: true,
                category: String::new(),
            },
            instructions: "use calculator tool".into(),
        });
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            reg,
            ToolRegistry::new(),
            Arc::new(AdapterRegistry::new()),
        );
        let session = agent
            .trunk
            .get_or_create(&SessionKey::local_tui(), "user".into(), None);
        agent
            .process_message(&session, "help me calc 1+1")
            .await
            .unwrap();
        let history = session.history.lock().await.clone();
        let blocks = agent.context_blocks(&history).await;
        let keys: Vec<&str> = blocks.iter().map(|block| block.key.as_str()).collect();
        assert!(keys.contains(&"base"), "base prompt block: {keys:?}");
        assert!(keys.contains(&"skills"), "skill metadata block: {keys:?}");
        assert!(
            keys.contains(&"triggered:calc"),
            "triggered skill block: {keys:?}"
        );
        assert!(
            keys.contains(&"orchestration"),
            "orchestration block: {keys:?}"
        );
        assert!(
            keys.iter().any(|key| key.starts_with("history:user")),
            "history aggregated per role: {keys:?}"
        );
        let total: usize = blocks.iter().map(|block| block.tokens).sum();
        assert!(total > 0, "blocks carry token estimates");
        let system = blocks
            .iter()
            .find(|block| block.key == "triggered:calc")
            .expect("triggered block");
        assert!(system.content.contains("calculator"));
    }

    #[tokio::test]
    async fn skill_keyword_loads_instructions() {
        let mut reg = SkillRegistry::new();
        reg.register(crate::skill::Skill {
            metadata: crate::skill::SkillMetadata {
                name: "calc".into(),
                description: "d".into(),
                keywords: vec!["calc".into()],
                always: false,
                enabled: true,
                category: String::new(),
            },
            instructions: "use calculator tool".into(),
        });
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            reg,
            ToolRegistry::new(),
            Arc::new(AdapterRegistry::new()),
        );
        let prompt = agent.build_system_prompt("help me calc 1+1").await;
        assert!(prompt.contains("Available skills"));
        assert!(prompt.contains("Triggered skill: calc"));
        assert!(prompt.contains("calculator"));
    }

    #[tokio::test]
    async fn skill_directory_hot_reloads_updates_additions_and_deletions() {
        let dir = std::env::temp_dir().join(format!("echo-skill-reload-{}", uuid::Uuid::new_v4()));
        let style_dir = dir.join("style");
        std::fs::create_dir_all(&style_dir).unwrap();
        let style_path = style_dir.join("SKILL.md");
        std::fs::write(
            &style_path,
            "---\nname: style\ndescription: style\nmetadata:\n  always: true\n---\nold instructions",
        )
        .unwrap();

        let config = AgentConfig {
            skills_dir: dir.display().to_string(),
            ..Default::default()
        };
        let initial = SkillRegistry::discover(&config.skills_dir).unwrap();
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = Arc::new(Agent::new(
            provider,
            config,
            initial,
            ToolRegistry::new(),
            Arc::new(AdapterRegistry::new()),
        ));

        let first_prompt = agent.build_system_prompt("plain input").await;
        assert!(first_prompt.contains("old instructions"));
        agent.start_skill_reload_task().await;

        std::fs::write(
            &style_path,
            "---\nname: style\ndescription: updated\nmetadata:\n  always: true\n---\nnew instructions",
        )
        .unwrap();
        let extra_dir = dir.join("extra");
        std::fs::create_dir_all(&extra_dir).unwrap();
        std::fs::write(
            extra_dir.join("SKILL.md"),
            "---\nname: extra\ndescription: extra\nkeywords: [extra]\n---\nextra instructions",
        )
        .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                let skills = agent.skills.lock().await;
                let updated = skills
                    .get("style")
                    .is_some_and(|skill| skill.instructions == "new instructions");
                let added = skills.get("extra").is_some();
                drop(skills);
                if updated && added {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("modified and added skills should hot reload");

        let second_prompt = agent.build_system_prompt("plain input").await;
        assert!(second_prompt.contains("new instructions"));
        assert!(!second_prompt.contains("old instructions"));

        std::fs::remove_file(&style_path).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                if agent.skills.lock().await.get("style").is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("deleted skill should be removed by hot reload");

        agent.shutdown().await;
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── Tool calling loop ──────────────────────────────────────────────────

    /// LLM provider that replays a fixed script of responses.
    struct ScriptedProvider {
        script: tokio::sync::Mutex<std::collections::VecDeque<ChatResponse>>,
        calls: Arc<AtomicUsize>,
        requests: tokio::sync::Mutex<Vec<ChatRequest>>,
    }

    impl ScriptedProvider {
        fn new(script: Vec<ChatResponse>) -> Self {
            Self {
                script: tokio::sync::Mutex::new(script.into()),
                calls: Arc::new(AtomicUsize::new(0)),
                requests: tokio::sync::Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }
        fn default_model(&self) -> &str {
            "mock-model"
        }
        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.messages[0].role, crate::llm::ChatRole::System);
            self.requests.lock().await.push(request.clone());
            self.script
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| LlmError::Config("script exhausted".into()))
        }
        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<(), LlmError> {
            Ok(())
        }
    }

    /// A tool that returns a canned result.
    struct MockTool {
        name: &'static str,
        result: String,
    }

    #[async_trait::async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "mock tool"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _arguments: serde_json::Value) -> Result<String, ToolError> {
            Ok(self.result.clone())
        }
    }

    fn tool_call(id: &str, name: &str) -> ChatResponse {
        ChatResponse {
            content: Some(format!("calling {name}")),
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: "{}".into(),
            }],
            usage: Usage::default(),
        }
    }

    #[tokio::test]
    async fn tool_calling_loop_executes_and_finishes() {
        let script = vec![
            tool_call("call_1", "mock_tool"),
            ChatResponse {
                content: Some("final answer".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            },
        ];
        let provider = Arc::new(ScriptedProvider::new(script));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MockTool {
            name: "mock_tool",
            result: "tool output".into(),
        }));

        let agent = Agent::new(
            provider.clone(),
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let key = SessionKey::local_tui();
        let session = agent.trunk.get_or_create(&key, "user".into(), None);
        let reply = agent
            .process_message(&session, "use the tool")
            .await
            .unwrap();
        assert_eq!(reply, "final answer");
        assert_eq!(provider.call_count(), 2);
    }

    #[tokio::test]
    async fn tool_loop_replays_reasoning_content_on_the_next_request() {
        let mut first = tool_call("call_1", "mock_tool");
        first.reasoning_content = Some("先调用工具，再根据结果回答".into());
        let provider = Arc::new(ScriptedProvider::new(vec![
            first,
            ChatResponse {
                content: Some("done".into()),
                reasoning_content: Some("工具返回成功".into()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
            },
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MockTool {
            name: "mock_tool",
            result: "ok".into(),
        }));
        let agent = Agent::new(
            provider.clone(),
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let session = agent
            .trunk
            .get_or_create(&SessionKey::local_tui(), "user".into(), None);

        assert_eq!(
            agent.process_message(&session, "run").await.unwrap(),
            "done"
        );
        let requests = provider.requests.lock().await;
        let replayed = requests[1]
            .messages
            .iter()
            .find(|message| message.tool_calls.is_some())
            .expect("assistant tool-call message");
        assert_eq!(
            replayed.reasoning_content.as_deref(),
            Some("先调用工具，再根据结果回答")
        );
    }

    #[tokio::test]
    async fn qq_hook_retries_backend_only_reply_until_send_tool_is_called() {
        let script = vec![
            ChatResponse {
                content: Some("backend only".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            },
            ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![ToolCall {
                    id: "send_1".into(),
                    name: "send_private_msg".into(),
                    arguments: r#"{"user_id":123456,"content":"收到"}"#.into(),
                }],
                usage: Usage::default(),
            },
            ChatResponse {
                content: Some("delivered".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            },
        ];
        let provider = Arc::new(ScriptedProvider::new(script));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MockTool {
            name: "send_private_msg",
            result: "private message sent".into(),
        }));
        let agent = Agent::new(
            provider.clone(),
            AgentConfig::default(),
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let key = SessionKey {
            platform: "qq".into(),
            scope: "dm".into(),
            scope_id: String::new(),
            user_id: "123456".into(),
        };
        let session = agent.trunk.get_or_create(&key, "tester".into(), None);
        let hook = r#"<qq_message_hook>
{"channel":{"type":"private"},"sender":{"user_id":"123456"},"content":"你好"}
</qq_message_hook>"#;

        let reply = agent.process_message(&session, hook).await.unwrap();

        assert_eq!(reply, "delivered");
        assert_eq!(provider.call_count(), 3);
        let requests = provider.requests.lock().await;
        assert!(requests[1].messages.iter().any(|message| {
            message.content.contains("<backend_delivery_correction>")
                && message.content.contains("user_id=123456")
        }));
        let history = session.history.lock().await;
        // Event-sourced history: user hook + synthesized assistant tool_use +
        // tool result + assistant reply. The tool_use must precede its result
        // so the provider never sees an orphaned tool_result after a reload.
        assert_eq!(history.len(), 4);
        assert_eq!(
            history[1].role,
            crate::llm::ChatRole::Assistant,
            "tool_use is model-visible before its tool_result"
        );
        let tool_use = history[1].tool_calls.as_ref().unwrap();
        assert_eq!(tool_use[0].name, "send_private_msg");
        assert_eq!(history[2].role, crate::llm::ChatRole::Tool);
        assert_eq!(
            history[2].tool_call_id.as_deref(),
            Some(tool_use[0].id.as_str())
        );
        assert_eq!(history[3].content, "delivered");
    }

    #[test]
    fn sequence_parsing_only_accepts_structured_markers() {
        let hook = r#"<qq_message_hook>
{"channel":{"type":"private"},"message_sequence":7,"content":"hi"}
</qq_message_hook>"#;
        assert_eq!(structured_message_sequence(hook), Some(7));
        let backend = r#"<backend_message_hook>{"message_sequence":3}</backend_message_hook>"#;
        assert_eq!(structured_message_sequence(backend), Some(3));
        let timer = r#"<timer_event>{"message_sequence":9}</timer_event>"#;
        assert_eq!(structured_message_sequence(timer), Some(9));
        let background =
            r#"<background_task_event>{"message_sequence":11}</background_task_event>"#;
        assert_eq!(structured_message_sequence(background), Some(11));

        // Ordinary conversation containing braces must NOT be parsed.
        assert_eq!(
            structured_message_sequence("请对比 {\"a\":1} 和 {\"b\":2}"),
            None
        );
        assert_eq!(
            structured_message_sequence("message_sequence: 42 in plain text"),
            None
        );
        assert_eq!(structured_message_sequence("const x = {n: 1};"), None);
        // The marker must be at the very start.
        assert_eq!(
            structured_message_sequence(
                "note: <qq_message_hook>{\"message_sequence\":1}</qq_message_hook>"
            ),
            None
        );
    }

    #[test]
    fn extract_input_images_reads_hook_payload() {
        let hook = r#"<qq_message_hook>
        {"channel":{"type":"private"},"message_sequence":7,"content":"hi","images":["https://example.com/a.png","data:image/png;base64,AAA"]}
        </qq_message_hook>"#;
        assert_eq!(
            extract_input_images(hook),
            vec![
                "https://example.com/a.png".to_string(),
                "data:image/png;base64,AAA".to_string()
            ]
        );
        // 无图 hook 返回空
        let plain = r#"<qq_message_hook>{"message_sequence":1,"content":"hi","images":[]}</qq_message_hook>"#;
        assert!(extract_input_images(plain).is_empty());
        // 普通文本不解析
        assert!(extract_input_images("看一下这张图片 https://example.com/a.png").is_empty());
    }

    #[test]
    fn user_message_event_roundtrips_images() {
        let hook = r#"<qq_message_hook>{"message_sequence":3,"content":"看图","images":["https://example.com/a.png"]}</qq_message_hook>"#;
        let event = echo_session::event::UserMessage {
            content: hook.into(),
            timestamp: 0,
            message_sequence: Some(3),
            source: None,
            images: extract_input_images(hook),
        };
        let json =
            serde_json::to_string(&echo_session::SessionEvent::UserMessage(event.clone())).unwrap();
        let back: echo_session::SessionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, echo_session::SessionEvent::UserMessage(event));
    }

    #[test]
    fn probe_config_resolves_active_profile_and_named_profiles() {
        use crate::config::ApiProfile;
        let mut cfg = crate::config::AgentConfig::default();
        cfg.provider = "deepseek".into();
        cfg.model = "deepseek-v4-flash".into();
        cfg.base_url = "https://api.deepseek.com/anthropic".into();
        cfg.api_key = "sk-xxx".into();
        cfg.active_api = "deepseek".into();
        cfg.api_profiles.push(ApiProfile {
            name: "deepseek".into(),
            provider: "deepseek".into(),
            model: "deepseek-v4-flash".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: "sk-xxx".into(),
            thinking: crate::config::ThinkingMode::Enabled,
            reasoning_effort: crate::config::ReasoningEffort::Max,
        });
        cfg.api_profiles.push(ApiProfile {
            name: "openai".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            thinking: crate::config::ThinkingMode::Disabled,
            reasoning_effort: crate::config::ReasoningEffort::Low,
        });

        // 默认配置（name=''）：合并激活 profile → provider 非空。
        let probe = resolve_probe_config(&cfg, "").expect("default probe resolves");
        assert_eq!(probe.provider, "deepseek");
        assert!(!probe.api_key.is_empty(), "api key resolved");

        // 指定 profile：使用该 profile 的值。
        let probe = resolve_probe_config(&cfg, "openai").expect("openai probe resolves");
        assert_eq!(probe.provider, "openai");
        assert_eq!(probe.model, "gpt-4o");

        // 不存在的 profile → 明确报错。
        let err = resolve_probe_config(&cfg, "nope").unwrap_err();
        assert!(err.contains("profile not found"));
    }

    #[test]
    fn probe_config_reports_missing_provider() {
        let cfg = crate::config::AgentConfig::default();
        let err = resolve_probe_config(&cfg, "").unwrap_err();
        assert!(
            err.contains("provider"),
            "error must mention provider: {err}"
        );
    }

    #[test]
    fn qq_delivery_rejects_wrong_target_but_allows_repeat_delivery() {
        let policy = QqDeliveryPolicy;
        let targets = vec![DeliveryTarget::Direct {
            user_id: "123456".into(),
        }];
        let mut delivered = std::collections::HashSet::new();
        let wrong = ToolCall {
            id: "1".into(),
            name: "send_private_msg".into(),
            arguments: r#"{"user_id":999,"content":"x"}"#.into(),
        };
        assert!(policy
            .validate_delivery_call(&targets, &mut delivered, &wrong)
            .unwrap_err()
            .contains("not declared"));

        let correct = ToolCall {
            id: "2".into(),
            name: "send_private_msg".into(),
            arguments: r#"{"user_id":123456,"content":"x"}"#.into(),
        };
        // 第一次投递：声明目标匹配，记录 delivered。
        let key = policy
            .validate_delivery_call(&targets, &mut delivered, &correct)
            .unwrap()
            .unwrap();
        assert!(delivered.contains(&key));
        // 同一目标再次投递：不再拦截（回复条数由 agent 决定）。
        let again = ToolCall {
            id: "3".into(),
            name: "send_private_msg".into(),
            arguments: r#"{"user_id":123456,"content":"y"}"#.into(),
        };
        assert!(policy
            .validate_delivery_call(&targets, &mut delivered, &again)
            .is_ok());
        // pending() 在首次投递后为空：reminder 不再触发。
        let plan = DeliveryPlan {
            targets: targets.clone(),
        };
        assert!(plan.pending(&delivered).is_empty());
    }

    #[test]
    fn background_delivery_plan_tracks_backend_private_and_group_targets() {
        let input = r#"<background_task_event>{
            "deliveries": [
                {"target":{"kind":"backend","session_id":"local:tui::local_user"}},
                {"target":{"kind":"qq_private","user_id":"100"}},
                {"target":{"kind":"qq_group","group_id":"200"}}
            ]
        }</background_task_event>"#;
        let plan = DeliveryPlan::from_input(input).unwrap();
        let mut delivered = std::collections::HashSet::new();
        let calls = [
            ToolCall {
                id: "backend".into(),
                name: "send_backend_message".into(),
                arguments: r#"{"session_id":"local:tui::local_user","content":"a"}"#.into(),
            },
            ToolCall {
                id: "private".into(),
                name: "send_private_msg".into(),
                arguments: r#"{"user_id":100,"content":"b"}"#.into(),
            },
            ToolCall {
                id: "group".into(),
                name: "send_group_msg".into(),
                arguments: r#"{"group_id":"200","content":"c"}"#.into(),
            },
        ];
        let targets = plan.targets.as_slice();
        for call in &calls {
            let key = QqDeliveryPolicy
                .validate_delivery_call(targets, &mut delivered, call)
                .unwrap()
                .unwrap();
            delivered.insert(key);
        }
        assert!(plan.pending(&delivered).is_empty());
    }

    #[tokio::test]
    async fn max_tool_iterations_returns_error() {
        let cfg = AgentConfig {
            max_tool_iterations: 2,
            ..Default::default()
        };
        let script = vec![
            tool_call("c1", "mock_tool"),
            tool_call("c2", "mock_tool"),
            tool_call("c3", "mock_tool"),
        ];
        let provider = Arc::new(ScriptedProvider::new(script));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MockTool {
            name: "mock_tool",
            result: "x".into(),
        }));

        let agent = Agent::new(
            provider,
            cfg,
            SkillRegistry::new(),
            tools,
            Arc::new(AdapterRegistry::new()),
        );
        let key = SessionKey::local_tui();
        let session = agent.trunk.get_or_create(&key, "user".into(), None);
        let err = agent.process_message(&session, "loop").await.unwrap_err();
        assert!(err.to_string().contains("max tool iterations"));
    }

    #[tokio::test]
    async fn tool_execution_failure_is_surfaced_to_llm() {
        let script = vec![
            tool_call("call_1", "missing_tool"),
            ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            },
        ];
        let provider = Arc::new(ScriptedProvider::new(script));
        let agent = Agent::new(
            provider,
            AgentConfig::default(),
            SkillRegistry::new(),
            ToolRegistry::new(),
            Arc::new(AdapterRegistry::new()),
        );
        let key = SessionKey::local_tui();
        let session = agent.trunk.get_or_create(&key, "user".into(), None);
        let reply = agent.process_message(&session, "hi").await.unwrap();
        assert_eq!(reply, "done");
    }

    #[tokio::test]
    async fn subagent_runs_without_tools_and_returns_to_parent() {
        let script = vec![
            ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![ToolCall {
                    id: "subagent_1".into(),
                    name: "run_subagent".into(),
                    arguments: serde_json::json!({
                        "task": "Compare two approaches",
                        "context": "Only use the supplied facts"
                    })
                    .to_string(),
                }],
                usage: Usage::default(),
            },
            ChatResponse {
                content: Some("isolated result".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            },
            ChatResponse {
                content: Some("parent final".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            },
        ];
        let provider = Arc::new(ScriptedProvider::new(script));
        let agent = test_agent(provider.clone());
        let session = agent
            .trunk
            .get_or_create(&SessionKey::local_tui(), "user".into(), None);

        let reply = agent
            .process_message(&session, "delegate this")
            .await
            .unwrap();

        assert_eq!(reply, "parent final");
        let requests = provider.requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert!(
            requests[1].tools.is_none(),
            "subagent must not receive tools"
        );
        assert!(requests[1]
            .messages
            .iter()
            .any(|message| message.content.contains("Compare two approaches")));
        let parent_tools = requests[0].tools.as_ref().unwrap();
        assert!(parent_tools
            .iter()
            .any(|definition| definition.name == "run_subagent"));
    }

    #[tokio::test]
    async fn timers_are_scoped_to_the_originating_session_and_can_be_cancelled() {
        let provider = Arc::new(MockProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "ok".into(),
        });
        let agent = test_agent(provider);
        let schedule = ToolCall {
            id: "schedule_1".into(),
            name: "schedule_timer".into(),
            arguments: serde_json::json!({
                "delay_seconds": 3600,
                "task": "send a reminder"
            })
            .to_string(),
        };
        let scheduled = agent
            .run_tool("local:tui::one", "test-branch", &schedule)
            .await;
        let timer_id = serde_json::from_str::<serde_json::Value>(&scheduled.text).unwrap()
            ["timer_id"]
            .as_str()
            .unwrap()
            .to_string();

        let listed = agent
            .run_tool(
                "local:tui::one",
                "test-branch",
                &ToolCall {
                    id: "list_1".into(),
                    name: "list_timers".into(),
                    arguments: "{}".into(),
                },
            )
            .await;
        assert!(listed.text.contains(&timer_id));

        let cancel = ToolCall {
            id: "cancel_1".into(),
            name: "cancel_timer".into(),
            arguments: serde_json::json!({ "timer_id": timer_id }).to_string(),
        };
        let rejected = agent
            .run_tool("local:tui::two", "test-branch", &cancel)
            .await;
        assert!(rejected.text.contains("not found in the current session"));
        let cancelled = agent
            .run_tool("local:tui::one", "test-branch", &cancel)
            .await;
        assert!(cancelled.text.contains("cancelled"));
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn due_timer_reenters_the_original_session() {
        let script = vec![
            ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![ToolCall {
                    id: "schedule_now".into(),
                    name: "schedule_timer".into(),
                    arguments: serde_json::json!({
                        "delay_seconds": 0,
                        "task": "perform the due task"
                    })
                    .to_string(),
                }],
                usage: Usage::default(),
            },
            ChatResponse {
                content: Some("timer scheduled".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            },
            ChatResponse {
                content: Some("timer completed".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
            },
        ];
        let provider = Arc::new(ScriptedProvider::new(script));
        let agent = Arc::new(test_agent(provider.clone()));
        agent.start_orchestration_task();
        let session = agent
            .trunk
            .get_or_create(&SessionKey::local_tui(), "user".into(), None);

        let reply = agent
            .process_message(&session, "remind me now")
            .await
            .unwrap();
        assert_eq!(reply, "timer scheduled");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if provider.call_count() == 3 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("due timer should invoke the agent");

        let history = session.history.lock().await;
        // Event-sourced history: user + synthesized assistant tool_use +
        // tool result + "timer scheduled" + timer-event user + "timer completed".
        assert_eq!(history.len(), 6);
        assert_eq!(history[1].role, crate::llm::ChatRole::Assistant);
        assert_eq!(
            history[1].tool_calls.as_ref().unwrap()[0].name,
            "schedule_timer"
        );
        assert_eq!(history[2].role, crate::llm::ChatRole::Tool);
        assert_eq!(
            history[2].tool_call_id.as_deref(),
            Some(history[1].tool_calls.as_ref().unwrap()[0].id.as_str())
        );
        assert_eq!(history[3].content, "timer scheduled");
        assert!(history[4].content.starts_with("<timer_event>"));
        assert_eq!(history[5].content, "timer completed");
        drop(history);
        agent.shutdown().await;
    }
}
