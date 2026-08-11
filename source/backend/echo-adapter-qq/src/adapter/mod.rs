//! QQ/OneBot adapter — wraps echo-server and implements the Adapter trait.
//!
//! Filter/gate management and TOML persistence live in `filter.rs`.

mod filter;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use echo_adapter::filter::FilterPipeline;
use echo_adapter::traits::{Adapter, AdapterConnectionState, AdapterError, AdapterInfo, GateMode};
use echo_adapter::types::{AdapterEvent, ChannelType, IncomingMessage, MessageTarget, SendResult};
use echo_core::{Event as OneBotEvent, MessageEvent, Segment};
use echo_server::{ConnCallback, Context, HandlerRegistry, Server, ServerConfig};
use tokio::sync::mpsc;

use crate::config::QqAdapterConfig;
use crate::napcat::NapCatState;
use crate::NapCatClient;

const NAPCAT_INITIAL_CHECK_DELAY: Duration = Duration::from_secs(2);
const NAPCAT_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Gating mode — mutually exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QqGateMode {
    /// No filtering — everyone can interact.
    None,
    /// Only allowlisted users/groups can interact.
    Allowlist,
    /// Denylisted users/groups are blocked; everyone else allowed.
    Denylist,
}

/// Shared inner state — lives in an Arc so the server task can reference it.
/// Fields are `pub(crate)` so the message handler (handler.rs) can use them.
pub(crate) struct QqInner {
    pub(crate) config: QqAdapterConfig,
    pub(crate) active_context: StdMutex<Option<Arc<Context>>>,
    pub(crate) running: AtomicBool,
    pub(crate) started_at: StdMutex<Option<i64>>,
    pub(crate) connected: AtomicBool,
    pub(crate) self_id: StdMutex<Option<String>>,
    pub(crate) subscribers: StdMutex<Vec<mpsc::UnboundedSender<AdapterEvent>>>,
    pub(crate) filter: StdMutex<Option<Arc<FilterPipeline>>>,
    pub(crate) shutdown_tx: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
    pub(crate) napcat_config_task: StdMutex<Option<tokio::task::AbortHandle>>,
    pub(crate) tracker: echo_server::ConnectionTracker,
    /// One-way inbound hook (set externally before start). Adapters depend
    /// only on echo-adapter abstractions, not on echo-agent itself.
    pub(crate) message_hook: StdMutex<Option<Arc<dyn echo_adapter::InboundMessageHook>>>,
    /// Extra handlers to register alongside the internal QQ handler.
    pub(crate) extra_handlers: StdMutex<Vec<Box<dyn echo_server::Handler>>>,
    /// Runtime allowlist overrides (takes precedence over config file values).
    pub(crate) runtime_allowlist_users: StdMutex<Vec<i64>>,
    pub(crate) runtime_allowlist_groups: StdMutex<Vec<i64>>,
    /// Runtime denylist overrides.
    pub(crate) runtime_denylist_users: StdMutex<Vec<i64>>,
    pub(crate) runtime_denylist_groups: StdMutex<Vec<i64>>,
    /// Current gating mode (mutually exclusive).
    pub(crate) gate_mode: StdMutex<QqGateMode>,
    /// Shared config store for persisting gate/filter changes.
    pub(crate) config_store: StdMutex<Option<echo_adapter::ConfigStore>>,
    /// Resolves local file paths into paths/URLs visible to NapCat, so
    /// `send_file` works even when the agent and NapCat are on different
    /// hosts (agent on host, NapCat in Docker).
    pub(crate) file_bridge: crate::file_bridge::FileBridge,
    /// Cache of group_id → group_name, refreshed lazily when a group message
    /// arrives without a known name. Lets inbound messages carry the group
    /// name even though OneBot group events only include the group_id.
    pub(crate) group_names: dashmap::DashMap<i64, String>,
}

/// Wrapper to make a boxed handler satisfy the `Handler` trait constraint.
struct BoxedHandler(Box<dyn echo_server::Handler>);

#[async_trait]
impl echo_server::Handler for BoxedHandler {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn priority(&self) -> i32 {
        self.0.priority()
    }
    async fn handle(
        &self,
        ctx: &echo_server::Context,
        event: &OneBotEvent,
    ) -> echo_server::HandleResult {
        self.0.handle(ctx, event).await
    }
}

/// The QQ adapter — wraps a reverse-WebSocket server (echo-server)
/// and bridges OneBot v11 events into the agent.
pub struct QqAdapter {
    name: String,
    inner: Arc<QqInner>,
}

impl QqAdapter {
    pub fn new(config: QqAdapterConfig) -> Self {
        // Seed runtime override fields from config.
        let au = config.filter.allowlist.user_ids.clone();
        let ag = config.filter.allowlist.group_ids.clone();
        let du = config.filter.denylist.user_ids.clone();
        let dg = config.filter.denylist.group_ids.clone();
        let gate_mode = match config.gate.mode.as_str() {
            "allowlist" => QqGateMode::Allowlist,
            "denylist" => QqGateMode::Denylist,
            _ => QqGateMode::None,
        };
        // Clone the NapCat bridge parameters before `config` is moved into the
        // struct initializer below.
        let napcat_onebot_url = config.napcat_onebot_url.clone();
        let napcat_host = config.napcat_host.clone();
        let napcat_container = config.napcat_container.clone();
        let napcat_container_data_dir = config.napcat_container_data_dir.clone();
        let adapter = Self {
            name: "qq".into(),
            inner: Arc::new(QqInner {
                config,
                active_context: StdMutex::new(None),
                running: AtomicBool::new(false),
                started_at: StdMutex::new(None),
                connected: AtomicBool::new(false),
                self_id: StdMutex::new(None),
                subscribers: StdMutex::new(Vec::new()),
                filter: StdMutex::new(None), // built below via rebuild_filter_pipeline
                shutdown_tx: StdMutex::new(None),
                napcat_config_task: StdMutex::new(None),
                tracker: echo_server::ConnectionTracker::default(),
                message_hook: StdMutex::new(None),
                extra_handlers: StdMutex::new(Vec::new()),
                runtime_allowlist_users: StdMutex::new(au),
                runtime_allowlist_groups: StdMutex::new(ag),
                runtime_denylist_users: StdMutex::new(du),
                runtime_denylist_groups: StdMutex::new(dg),
                gate_mode: StdMutex::new(gate_mode),
                config_store: StdMutex::new(None),
                file_bridge: crate::file_bridge::FileBridge::new(
                    &napcat_onebot_url,
                    &napcat_host,
                    &napcat_container,
                    &napcat_container_data_dir,
                ),
                group_names: dashmap::DashMap::new(),
            }),
        };
        adapter.rebuild_filter_pipeline();
        adapter
    }
    async fn maintain_napcat_connection(
        inner: Arc<QqInner>,
        napcat: NapCatClient,
        ws_url: String,
        token: String,
        initial_delay: Duration,
        retry_interval: Duration,
    ) {
        tokio::time::sleep(initial_delay).await;

        let mut last_state = None;
        while inner.running.load(Ordering::SeqCst) && !inner.connected.load(Ordering::SeqCst) {
            match napcat.check().await {
                NapCatState::Online { user_id, nickname } => {
                    if last_state != Some("online") {
                        tracing::info!(
                            %user_id,
                            %nickname,
                            %ws_url,
                            "NapCat online - configuring reverse WebSocket"
                        );
                    }
                    if let Err(e) = napcat.configure_reverse_ws(&ws_url, &token).await {
                        tracing::warn!(error = %e, "NapCat auto-config failed; will retry");
                    }
                    last_state = Some("online");
                }
                NapCatState::WaitingForLogin => {
                    if last_state != Some("waiting") {
                        tracing::info!("NapCat running, waiting for QQ login");
                    }
                    last_state = Some("waiting");
                }
                NapCatState::Unreachable => {
                    if last_state != Some("unreachable") {
                        tracing::info!("NapCat API is not reachable; waiting for it to start");
                    }
                    last_state = Some("unreachable");
                }
            }

            tokio::time::sleep(retry_interval).await;
        }
    }

    /// Attach the one-way inbound hook before starting the adapter.
    pub fn set_message_hook(&self, hook: Arc<dyn echo_adapter::InboundMessageHook>) {
        *self
            .inner
            .message_hook
            .lock()
            .expect("message hook poisoned") = Some(hook);
    }

    /// Register an additional handler (e.g. Echo, Help, Admin).
    pub fn add_handler(&self, handler: Box<dyn echo_server::Handler>) {
        self.inner
            .extra_handlers
            .lock()
            .expect("handlers poisoned")
            .push(handler);
    }

    /// Get ALL groups without gate-mode filtering (privileged — used by TUI).
    pub async fn get_all_groups(&self) -> Result<Vec<(i64, String)>, String> {
        // Add diagnostic logging
        let connected = self.inner.connected.load(Ordering::SeqCst);
        let running = self.inner.running.load(Ordering::SeqCst);
        let has_context = self
            .inner
            .active_context
            .lock()
            .ok()
            .map(|g| g.is_some())
            .unwrap_or(false);

        tracing::info!(
            "get_all_groups called - connected: {}, running: {}, has_context: {}, self_id: {:?}",
            connected,
            running,
            has_context,
            self.inner.self_id.lock().ok()
        );

        // First check if QQ adapter is running
        if !running {
            return Err("QQ适配器未启动，请检查配置".to_string());
        }

        let napcat = NapCatClient::with_onebot_url(
            &self.inner.config.napcat_webui_url,
            &self.inner.config.napcat_onebot_url,
        );
        match napcat.get_group_list().await {
            Ok(groups) => return Ok(groups),
            Err(error) => {
                tracing::debug!(%error, "OneBot HTTP group list unavailable; trying WebSocket");
            }
        }

        // Fall back to reverse WebSocket when the HTTP API is unavailable.
        if !connected {
            let self_id = self.inner.self_id.lock().ok().and_then(|g| g.clone());
            return Err(format!(
                "QQ WebSocket未连接{}，请先使用 /qq login 登录",
                self_id
                    .map(|id| format!(" (账号: {id})"))
                    .unwrap_or_default()
            ));
        }

        // Check for active context and provide guidance if missing
        let ctx_opt = self
            .inner
            .active_context
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?
            .clone();

        let ctx = match ctx_opt {
            Some(ctx) => ctx,
            None => {
                // QQ is connected but API context not yet activated
                return Err(
                    "QQ已连接但API上下文尚未激活，请发送任意消息到QQ群组来激活API功能".to_string(),
                );
            }
        };

        let request = echo_core::action::actions::get_group_list();
        let resp = ctx.send_api(request).await.map_err(|e| e.to_string())?;
        let groups: Vec<(i64, String)> = resp
            .data
            .as_array()
            .ok_or("unexpected response format")?
            .iter()
            .filter_map(|v| {
                let gid = v.get("group_id")?.as_i64()?;
                let name = v
                    .get("group_name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("(unknown)")
                    .to_string();
                Some((gid, name))
            })
            .collect();
        Ok(groups)
    }

    /// Get the QQ friend list via the active QQ connection.
    pub async fn get_friend_list(&self) -> Result<Vec<(i64, String)>, String> {
        // Add diagnostic logging
        let connected = self.inner.connected.load(Ordering::SeqCst);
        let running = self.inner.running.load(Ordering::SeqCst);
        let has_context = self
            .inner
            .active_context
            .lock()
            .ok()
            .map(|g| g.is_some())
            .unwrap_or(false);

        tracing::info!(
            "get_friend_list called - connected: {}, running: {}, has_context: {}, self_id: {:?}",
            connected,
            running,
            has_context,
            self.inner.self_id.lock().ok()
        );

        // First check if QQ adapter is running
        if !running {
            return Err("QQ适配器未启动，请检查配置".to_string());
        }

        let napcat = NapCatClient::with_onebot_url(
            &self.inner.config.napcat_webui_url,
            &self.inner.config.napcat_onebot_url,
        );
        match napcat.get_friend_list().await {
            Ok(friends) => return Ok(friends),
            Err(error) => {
                tracing::debug!(%error, "OneBot HTTP friend list unavailable; trying WebSocket");
            }
        }

        // Fall back to reverse WebSocket when the HTTP API is unavailable.
        if !connected {
            let self_id_opt = self.inner.self_id.lock().ok().and_then(|g| g.clone());
            return Err(format!(
                "QQ WebSocket未连接{}，请先使用 /qq login 登录",
                self_id_opt
                    .map(|id| format!(" (账号: {id})"))
                    .unwrap_or_default()
            ));
        }
        let ctx_opt = self
            .inner
            .active_context
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?
            .clone();

        let ctx = match ctx_opt {
            Some(ctx) => ctx,
            None => {
                // QQ is connected but API context not yet activated
                return Err(
                    "QQ已连接但API上下文尚未激活，请发送任意消息到QQ群组来激活API功能".to_string(),
                );
            }
        };

        let request = echo_core::action::actions::get_friend_list();
        let resp = ctx.send_api(request).await.map_err(|e| e.to_string())?;
        let friends: Vec<(i64, String)> = resp
            .data
            .as_array()
            .ok_or("unexpected response format")?
            .iter()
            .filter_map(|v| {
                let uid = v.get("user_id")?.as_i64()?;
                let name = v
                    .get("nickname")
                    .and_then(|n| n.as_str())
                    .unwrap_or("(unknown)")
                    .to_string();
                Some((uid, name))
            })
            .collect();
        Ok(friends)
    }

    /// Get the friend list visible under the current gate configuration.
    /// The TUI deliberately uses [`get_friend_list`](Self::get_friend_list)
    /// instead so administrators can still edit the complete list.
    pub async fn get_gated_friend_list(&self) -> Result<Vec<(i64, String)>, String> {
        let friends = self.get_friend_list().await?;
        Ok(self.apply_friend_gate(friends))
    }

    fn apply_friend_gate(&self, mut friends: Vec<(i64, String)>) -> Vec<(i64, String)> {
        let mode = self.get_gate_mode();
        if mode == QqGateMode::None {
            return friends;
        }

        let cfg = self.get_filter_config();
        let owner = self.inner.config.owner_qq;
        match mode {
            QqGateMode::Allowlist => {
                let allowed: std::collections::HashSet<i64> =
                    cfg.allowlist.user_ids.into_iter().collect();
                // An empty user allowlist does not restrict direct messages.
                if !allowed.is_empty() {
                    friends.retain(|(user_id, _)| *user_id == owner || allowed.contains(user_id));
                }
            }
            QqGateMode::Denylist => {
                let denied: std::collections::HashSet<i64> =
                    cfg.denylist.user_ids.into_iter().collect();
                friends.retain(|(user_id, _)| *user_id == owner || !denied.contains(user_id));
            }
            QqGateMode::None => {}
        }
        friends
    }

    /// Get group list via the active QQ connection.
    pub async fn get_group_list(&self) -> Result<Vec<(i64, String)>, String> {
        let ctx = self
            .inner
            .active_context
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?
            .clone()
            .ok_or("no QQ connection active")?;
        let request = echo_core::action::actions::get_group_list();
        let resp = ctx.send_api(request).await.map_err(|e| e.to_string())?;
        // The response data is an array of { group_id, group_name, ... }.
        let groups: Vec<(i64, String)> = resp
            .data
            .as_array()
            .ok_or("unexpected response format")?
            .iter()
            .filter_map(|v| {
                let gid = v.get("group_id")?.as_i64()?;
                let name = v
                    .get("group_name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("(unknown)")
                    .to_string();
                Some((gid, name))
            })
            .collect();

        // Apply gate-mode filtering to the group list.
        let mode = self.get_gate_mode();
        if mode != QqGateMode::None {
            let cfg = self.get_filter_config();
            let mut filtered = groups;
            match mode {
                QqGateMode::Allowlist => {
                    let allow_groups: std::collections::HashSet<i64> =
                        cfg.allowlist.group_ids.iter().cloned().collect();
                    filtered.retain(|(gid, _)| allow_groups.contains(gid));
                }
                QqGateMode::Denylist => {
                    let deny_groups: std::collections::HashSet<i64> =
                        cfg.denylist.group_ids.iter().cloned().collect();
                    if !deny_groups.is_empty() {
                        filtered.retain(|(gid, _)| !deny_groups.contains(gid));
                    }
                }
                QqGateMode::None => {}
            }
            return Ok(filtered);
        }
        Ok(groups)
    }

    /// Get group member info via the active QQ connection.
    pub async fn get_group_member_info(
        &self,
        group_id: i64,
        user_id: i64,
    ) -> Result<String, String> {
        // ── Gate mode check ──
        {
            let mode = self.get_gate_mode();
            if mode != QqGateMode::None {
                let cfg = self.get_filter_config();
                match mode {
                    QqGateMode::Allowlist => {
                        let allow_groups: std::collections::HashSet<i64> =
                            cfg.allowlist.group_ids.iter().cloned().collect();
                        if !allow_groups.is_empty() && !allow_groups.contains(&group_id) {
                            return Err(format!("group {group_id} is not allowlisted"));
                        }
                    }
                    QqGateMode::Denylist => {
                        let deny_groups: std::collections::HashSet<i64> =
                            cfg.denylist.group_ids.iter().cloned().collect();
                        if !deny_groups.is_empty() && deny_groups.contains(&group_id) {
                            return Err(format!("group {group_id} is denylisted"));
                        }
                    }
                    QqGateMode::None => {}
                }
            }
        }
        let _ = user_id; // reserved for future per-user gating

        let ctx = self
            .inner
            .active_context
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?
            .clone();
        let ctx = ctx.ok_or("no QQ connection active")?;
        let request = echo_core::action::actions::get_group_member_info(group_id, user_id);
        let resp = ctx.send_api(request).await.map_err(|e| e.to_string())?;
        serde_json::to_string(&resp.data).map_err(|e| e.to_string())
    }

    /// Upload a file to a group via the active QQ connection.
    ///
    /// `file_path` is a path on the machine running the agent (the host). It
    /// is transparently bridged to a location NapCat can read: container paths
    /// pass through, `docker cp` is attempted when available, and otherwise a
    /// local HTTP file server hands the file to NapCat via URL.
    /// `file_name` is the display name shown in QQ.
    pub async fn upload_group_file(
        &self,
        group_id: i64,
        file_path: &str,
        file_name: &str,
    ) -> Result<String, String> {
        let candidates = self
            .inner
            .file_bridge
            .resolve_all(file_path, file_name)
            .await?;
        let ctx = self
            .inner
            .active_context
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?
            .clone()
            .ok_or("no QQ connection active")?;

        let mut last_error = None;
        for remote in &candidates {
            let request =
                echo_core::action::actions::upload_group_file(group_id, remote, file_name);
            let resp = ctx.send_api(request).await.map_err(|e| e.to_string())?;
            if resp.is_ok() {
                tracing::info!(%group_id, %remote, "group file uploaded");
                return Ok(format!("file uploaded to group {group_id}"));
            }
            last_error = Some(
                resp.error_message()
                    .unwrap_or_else(|| "upload failed".into()),
            );
            tracing::debug!(%remote, error = last_error.as_deref().unwrap_or(""), "upload candidate failed; trying next");
        }
        Err(last_error.unwrap_or_else(|| "upload failed".into()))
    }

    /// Upload a file to a private chat via the active QQ connection.
    ///
    /// `file_path` is a path on the machine running the agent (the host). It
    /// is transparently bridged to a location NapCat can read (see
    /// [`upload_group_file`](Self::upload_group_file)).
    /// `file_name` is the display name shown in QQ.
    pub async fn upload_private_file(
        &self,
        user_id: i64,
        file_path: &str,
        file_name: &str,
    ) -> Result<String, String> {
        let candidates = self
            .inner
            .file_bridge
            .resolve_all(file_path, file_name)
            .await?;
        let ctx = self
            .inner
            .active_context
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?
            .clone()
            .ok_or("no QQ connection active")?;

        let mut last_error = None;
        for remote in &candidates {
            let request =
                echo_core::action::actions::upload_private_file(user_id, remote, file_name);
            let resp = ctx.send_api(request).await.map_err(|e| e.to_string())?;
            if resp.is_ok() {
                tracing::info!(%user_id, %remote, "private file uploaded");
                return Ok(format!("file uploaded to user {user_id}"));
            }
            last_error = Some(
                resp.error_message()
                    .unwrap_or_else(|| "upload failed".into()),
            );
            tracing::debug!(%remote, error = last_error.as_deref().unwrap_or(""), "upload candidate failed; trying next");
        }
        Err(last_error.unwrap_or_else(|| "upload failed".into()))
    }

    pub(crate) fn convert_message(
        event: &OneBotEvent,
        group_names: &dashmap::DashMap<i64, String>,
    ) -> Option<IncomingMessage> {
        let msg = event.as_message()?;
        if matches!(msg, MessageEvent::Unknown) {
            return None;
        }
        let content = msg.plain_text();
        if content.trim().is_empty() {
            return None;
        }
        let (channel, group_name) = if let Some(gid) = msg.group_id() {
            (
                ChannelType::Group {
                    group_id: gid.to_string(),
                },
                group_names.get(&gid).map(|name| name.clone()),
            )
        } else {
            (ChannelType::Direct, None)
        };
        Some(IncomingMessage {
            adapter_name: "qq".into(),
            platform: "qq".into(),
            user_id: msg.user_id().to_string(),
            user_name: msg.sender_nickname().to_string(),
            channel,
            group_name,
            content,
            timestamp: msg.timestamp(),
            at_me: msg.at_me(),
            metadata: serde_json::Value::Null,
        })
    }
}

#[async_trait]
impl Adapter for QqAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        "QQ / OneBot"
    }

    fn platform(&self) -> &str {
        "qq"
    }

    fn is_configured(&self) -> bool {
        !self.inner.config.server.bind_address.is_empty()
    }

    fn status_info(&self) -> AdapterInfo {
        let connected = self.inner.connected.load(Ordering::SeqCst);
        let running = self.inner.running.load(Ordering::SeqCst);
        AdapterInfo {
            name: self.name.clone(),
            display_name: "QQ / OneBot".into(),
            status: if !running {
                AdapterConnectionState::Stopped
            } else if connected {
                AdapterConnectionState::Connected
            } else {
                AdapterConnectionState::Disconnected
            },
            self_id: self.inner.self_id.lock().ok().and_then(|g| g.clone()),
            started_at: self.inner.started_at.lock().ok().and_then(|g| *g),
            platform: "qq".into(),
            configured: self.is_configured(),
        }
    }

    async fn start(&self) -> Result<(), AdapterError> {
        if self.inner.running.load(Ordering::SeqCst) {
            return Err(AdapterError::AlreadyRunning("qq".into()));
        }

        let server_cfg = ServerConfig {
            bind_address: self.inner.config.server.bind_address.clone(),
            access_token: if self.inner.config.server.access_token.is_empty() {
                None
            } else {
                Some(self.inner.config.server.access_token.clone())
            },
            heartbeat_interval: (self.inner.config.server.heartbeat_interval > 0)
                .then(|| Duration::from_secs(self.inner.config.server.heartbeat_interval)),
        };

        let inner = self.inner.clone();

        let bind_addr = server_cfg.bind_address.clone();

        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .map_err(|e| AdapterError::Internal(format!("bind {}: {e}", bind_addr)))?;

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *inner
            .shutdown_tx
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))? = Some(shutdown_tx);

        let inner_cb = inner.clone();
        let conn_cb: ConnCallback = Arc::new(move |connected: bool, self_id: i64| {
            if connected {
                *inner_cb.self_id.lock().expect("self_id poisoned") = Some(self_id.to_string());
                tracing::info!(%self_id, "NapCat client connected via reverse WebSocket");
            } else {
                tracing::warn!("NapCat client disconnected");
                // Clear active context when disconnected
                *inner_cb.active_context.lock().expect("ctx poisoned") = None;
            }
            inner_cb.connected.store(connected, Ordering::SeqCst);
            if let Some(ref hook) = *inner_cb.message_hook.lock().expect("message hook poisoned") {
                hook.on_connection_state("qq", connected, Some(self_id.to_string()));
            }
        });

        let mut registry = HandlerRegistry::new();
        // Cannot clone Box<dyn Handler>, so we take them out and re-create later if needed.
        // For now, take ownership and register via the wrapper.
        let extra: Vec<Box<dyn echo_server::Handler>> =
            std::mem::take(&mut *inner.extra_handlers.lock().expect("handlers poisoned"));
        for h in extra {
            registry.register(BoxedHandler(h));
        }
        registry.register(crate::handler::QqHandler::new(inner.clone()));

        inner.running.store(true, Ordering::SeqCst);
        *inner.started_at.lock().expect("started_at poisoned") =
            Some(chrono::Utc::now().timestamp());

        let server = Server::new(
            server_cfg,
            Arc::new(registry),
            inner.tracker.clone(),
            Some(conn_cb),
        );

        tokio::spawn(async move {
            let run_fut = server.run_with_listener(listener);
            tokio::select! {
                result = run_fut => {
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "QQ server terminated");
                    }
                }
                _ = &mut shutdown_rx => {
                    tracing::info!("QQ adapter shutdown requested");
                }
            }
            inner.running.store(false, Ordering::SeqCst);
        });

        // Keep trying until NapCat has logged in and established the reverse
        // WebSocket. Startup commonly happens before the QR login completes.
        let napcat_url = self.inner.config.napcat_webui_url.clone();
        let onebot_url = self.inner.config.napcat_onebot_url.clone();
        let napcat_host = self.inner.config.napcat_host.clone();
        let port = bind_addr.split(':').next_back().unwrap_or("3131");
        let ws_url = format!("ws://{napcat_host}:{port}");
        let token = self.inner.config.server.access_token.clone();
        let config_task = tokio::spawn(Self::maintain_napcat_connection(
            self.inner.clone(),
            NapCatClient::with_onebot_url(&napcat_url, &onebot_url),
            ws_url,
            token,
            NAPCAT_INITIAL_CHECK_DELAY,
            NAPCAT_RETRY_INTERVAL,
        ));
        *self
            .inner
            .napcat_config_task
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))? = Some(config_task.abort_handle());

        tracing::info!(address = %bind_addr, "QQ adapter started");
        Ok(())
    }

    async fn stop(&self) -> Result<(), AdapterError> {
        if !self.inner.running.load(Ordering::SeqCst) {
            return Err(AdapterError::NotRunning("qq".into()));
        }
        let tx = self
            .inner
            .shutdown_tx
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?
            .take();
        if let Some(tx) = tx {
            let _ = tx.send(());
        }
        if let Some(handle) = self
            .inner
            .napcat_config_task
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?
            .take()
        {
            handle.abort();
        }
        self.inner.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn send_message(
        &self,
        target: &MessageTarget,
        content: &str,
    ) -> Result<SendResult, AdapterError> {
        if target.adapter_name != "qq" {
            return Ok(SendResult {
                message_id: None,
                success: false,
                error: Some(format!(
                    "wrong adapter: expected 'qq', got '{}'",
                    target.adapter_name
                )),
            });
        }

        // ── Gate mode check ──
        {
            let mode = self.get_gate_mode();
            if mode != QqGateMode::None {
                let cfg = self.get_filter_config();
                let user_id: i64 = target.user_id.parse().unwrap_or(0);
                let group_id: Option<i64> = match &target.channel {
                    ChannelType::Group { group_id } => group_id.parse().ok(),
                    ChannelType::Direct => None,
                };
                // The owner is always exempt from outbound gating.
                let owner = self.inner.config.owner_qq;
                let is_owner = user_id != 0 && user_id == owner;

                match mode {
                    QqGateMode::Allowlist => {
                        let allow_users: std::collections::HashSet<i64> =
                            cfg.allowlist.user_ids.iter().cloned().collect();
                        let allow_groups: std::collections::HashSet<i64> =
                            cfg.allowlist.group_ids.iter().cloned().collect();
                        if !allow_users.is_empty()
                            && user_id != 0
                            && !is_owner
                            && !allow_users.contains(&user_id)
                        {
                            tracing::info!(%user_id, "send_message blocked: user not allowlisted");
                            return Ok(SendResult {
                                message_id: None,
                                success: false,
                                error: Some(format!("user {user_id} is not allowlisted")),
                            });
                        }
                        if !allow_groups.is_empty() {
                            if let Some(gid) = group_id {
                                if !allow_groups.contains(&gid) {
                                    tracing::info!(%gid, "send_message blocked: group not allowlisted");
                                    return Ok(SendResult {
                                        message_id: None,
                                        success: false,
                                        error: Some(format!("group {gid} is not allowlisted")),
                                    });
                                }
                            }
                        }
                    }
                    QqGateMode::Denylist => {
                        let deny_users: std::collections::HashSet<i64> =
                            cfg.denylist.user_ids.iter().cloned().collect();
                        let deny_groups: std::collections::HashSet<i64> =
                            cfg.denylist.group_ids.iter().cloned().collect();
                        if !deny_users.is_empty()
                            && user_id != 0
                            && !is_owner
                            && deny_users.contains(&user_id)
                        {
                            tracing::info!(%user_id, "send_message blocked: user denylisted");
                            return Ok(SendResult {
                                message_id: None,
                                success: false,
                                error: Some(format!("user {user_id} is denylisted")),
                            });
                        }
                        if !deny_groups.is_empty() {
                            if let Some(gid) = group_id {
                                if deny_groups.contains(&gid) {
                                    tracing::info!(%gid, "send_message blocked: group denylisted");
                                    return Ok(SendResult {
                                        message_id: None,
                                        success: false,
                                        error: Some(format!("group {gid} is denylisted")),
                                    });
                                }
                            }
                        }
                    }
                    // Never reached: the outer guard filters out None. A
                    // fallback arm keeps this robust if a new mode is added.
                    _ => {}
                }
            }
        }

        let ctx = self
            .inner
            .active_context
            .lock()
            .map_err(|e| AdapterError::Internal(e.to_string()))?
            .clone()
            .ok_or_else(|| AdapterError::SendFailed("no QQ connection active".into()))?;

        let result = match &target.channel {
            ChannelType::Direct => {
                let user_id: i64 = target
                    .user_id
                    .parse()
                    .map_err(|_| AdapterError::SendFailed("invalid user_id".into()))?;
                ctx.send_private_text(user_id, content).await
            }
            ChannelType::Group { group_id } => {
                let gid: i64 = group_id
                    .parse()
                    .map_err(|_| AdapterError::SendFailed("invalid group_id".into()))?;
                let user_id: i64 = if target.user_id.is_empty() {
                    0
                } else {
                    target
                        .user_id
                        .parse()
                        .map_err(|_| AdapterError::SendFailed("invalid user_id".into()))?
                };
                if user_id != 0 {
                    ctx.send_group_msg(
                        gid,
                        vec![Segment::at(target.user_id.clone()), Segment::text(content)],
                    )
                    .await
                } else {
                    ctx.send_group_text(gid, content).await
                }
            }
        };

        match result {
            Ok(resp) => {
                let message_id = resp
                    .data
                    .get("message_id")
                    .and_then(|v| v.as_i64())
                    .map(|id| id.to_string());
                Ok(SendResult {
                    message_id,
                    success: resp.is_ok(),
                    error: resp.error_message(),
                })
            }
            Err(e) => Ok(SendResult {
                message_id: None,
                success: false,
                error: Some(e.to_string()),
            }),
        }
    }

    fn subscribe(&self, tx: mpsc::UnboundedSender<AdapterEvent>) {
        if let Ok(mut subs) = self.inner.subscribers.lock() {
            subs.push(tx);
        }
    }

    fn filter_pipeline(&self) -> Option<&FilterPipeline> {
        // The filter pipeline is behind a mutex for runtime updates and can't
        // be borrowed through this trait method.  Return None — the real
        // filtering happens inside QqHandler::handle().
        None
    }

    async fn get_groups(&self) -> Result<Vec<(i64, String)>, AdapterError> {
        self.get_group_list().await.map_err(AdapterError::Internal)
    }

    async fn get_friend_list(&self) -> Result<Vec<(i64, String)>, AdapterError> {
        self.get_friend_list().await.map_err(AdapterError::Internal)
    }

    async fn get_all_groups(&self) -> Result<Vec<(i64, String)>, AdapterError> {
        self.get_all_groups().await.map_err(AdapterError::Internal)
    }

    async fn get_all_friends(&self) -> Result<Vec<(i64, String)>, AdapterError> {
        self.get_friend_list().await.map_err(AdapterError::Internal)
    }

    fn has_group_list(&self) -> bool {
        true
    }

    fn update_allowlist(&self, user_ids: Vec<String>, group_ids: Vec<String>) {
        let users: Vec<i64> = user_ids.iter().filter_map(|s| s.parse().ok()).collect();
        let groups: Vec<i64> = group_ids.iter().filter_map(|s| s.parse().ok()).collect();
        self.update_allowlist(users, groups);
    }

    fn update_denylist(&self, user_ids: Vec<String>, group_ids: Vec<String>) {
        let users: Vec<i64> = user_ids.iter().filter_map(|s| s.parse().ok()).collect();
        let groups: Vec<i64> = group_ids.iter().filter_map(|s| s.parse().ok()).collect();
        self.update_denylist(users, groups);
    }

    fn get_filter_info(&self) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
        let cfg = self.get_filter_config();
        (
            cfg.allowlist
                .user_ids
                .iter()
                .map(|id| id.to_string())
                .collect(),
            cfg.allowlist
                .group_ids
                .iter()
                .map(|id| id.to_string())
                .collect(),
            cfg.denylist
                .user_ids
                .iter()
                .map(|id| id.to_string())
                .collect(),
            cfg.denylist
                .group_ids
                .iter()
                .map(|id| id.to_string())
                .collect(),
        )
    }

    fn set_gate_mode(&self, mode: GateMode) {
        let m = match mode {
            GateMode::Allowlist => QqGateMode::Allowlist,
            GateMode::Denylist => QqGateMode::Denylist,
            GateMode::None => QqGateMode::None,
        };
        self.set_gate_mode(m);
    }

    fn get_gate_mode(&self) -> GateMode {
        match self.get_gate_mode() {
            QqGateMode::Allowlist => GateMode::Allowlist,
            QqGateMode::Denylist => GateMode::Denylist,
            QqGateMode::None => GateMode::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_core::Event;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn mount_login_info(server: &MockServer, user_id: i64) {
        Mock::given(method("POST"))
            .and(path("/get_login_info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "retcode": 0,
                "data": {"user_id": user_id, "nickname": "bot"}
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn configures_reverse_ws_when_login_completes_after_startup() {
        let onebot = MockServer::start().await;
        mount_login_info(&onebot, 0).await;

        let webui = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/network/wsReverse"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&webui)
            .await;

        let adapter = QqAdapter::new(QqAdapterConfig::default());
        adapter.inner.running.store(true, Ordering::SeqCst);
        let client = NapCatClient::with_onebot_url(&webui.uri(), &onebot.uri());
        let task = tokio::spawn(QqAdapter::maintain_napcat_connection(
            adapter.inner.clone(),
            client,
            "ws://host.docker.internal:3131".into(),
            String::new(),
            Duration::ZERO,
            Duration::from_millis(10),
        ));

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(webui.received_requests().await.unwrap().is_empty());

        onebot.reset().await;
        mount_login_info(&onebot, 10001).await;

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !webui.received_requests().await.unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reverse WebSocket configuration was not requested");

        adapter.inner.connected.store(true, Ordering::SeqCst);
        task.await.expect("maintenance task failed");
    }

    #[tokio::test]
    async fn all_groups_use_onebot_http_without_websocket_context() {
        let onebot = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/get_group_list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "retcode": 0,
                "data": [{"group_id": 987654, "group_name": "known group"}]
            })))
            .mount(&onebot)
            .await;

        let config = QqAdapterConfig {
            napcat_onebot_url: onebot.uri(),
            ..Default::default()
        };
        let adapter = QqAdapter::new(config);
        adapter.inner.running.store(true, Ordering::SeqCst);

        assert_eq!(
            adapter.get_all_groups().await.unwrap(),
            vec![(987654, "known group".into())]
        );
        assert!(!adapter.inner.connected.load(Ordering::SeqCst));
        assert!(adapter.inner.active_context.lock().unwrap().is_none());
    }

    #[test]
    fn friend_gate_respects_modes_and_owner_bypass() {
        let config = QqAdapterConfig {
            owner_qq: 999,
            ..Default::default()
        };
        let adapter = QqAdapter::new(config);
        let friends = || {
            vec![
                (111, "allowed".into()),
                (222, "other".into()),
                (999, "owner".into()),
            ]
        };

        assert_eq!(adapter.apply_friend_gate(friends()), friends());

        adapter.update_allowlist(vec![], vec![123]);
        adapter.set_gate_mode(QqGateMode::Allowlist);
        assert_eq!(
            adapter.apply_friend_gate(friends()),
            friends(),
            "a group-only allowlist does not restrict direct-message friends"
        );

        adapter.update_allowlist(vec![111], vec![]);
        assert_eq!(
            adapter.apply_friend_gate(friends()),
            vec![(111, "allowed".into()), (999, "owner".into())]
        );

        adapter.update_denylist(vec![222, 999], vec![]);
        adapter.set_gate_mode(QqGateMode::Denylist);
        assert_eq!(
            adapter.apply_friend_gate(friends()),
            vec![(111, "allowed".into()), (999, "owner".into())]
        );
    }

    #[tokio::test]
    async fn gated_friend_list_filters_onebot_results() {
        let onebot = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/get_friend_list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "retcode": 0,
                "data": [
                    {"user_id": 111, "nickname": "allowed"},
                    {"user_id": 222, "nickname": "blocked"}
                ]
            })))
            .mount(&onebot)
            .await;

        let config = QqAdapterConfig {
            napcat_onebot_url: onebot.uri(),
            ..Default::default()
        };
        let adapter = QqAdapter::new(config);
        adapter.inner.running.store(true, Ordering::SeqCst);
        adapter.update_allowlist(vec![111], vec![]);
        adapter.set_gate_mode(QqGateMode::Allowlist);

        assert_eq!(
            adapter.get_gated_friend_list().await.unwrap(),
            vec![(111, "allowed".into())]
        );
    }

    fn private_event(text: &str) -> Event {
        serde_json::from_value(serde_json::json!({
            "post_type": "message",
            "message_type": "private",
            "time": 1700000000,
            "self_id": 10001,
            "sub_type": "friend",
            "message_id": 42,
            "user_id": 123456,
            "message": [{"type": "text", "data": {"text": text}}],
            "raw_message": text,
            "font": 0,
            "sender": {"user_id": 123456, "nickname": "tester"}
        }))
        .unwrap()
    }

    fn group_event(text: &str, at_me: bool) -> Event {
        let mut segments = vec![serde_json::json!({"type": "text", "data": {"text": text}})];
        if at_me {
            segments.insert(
                0,
                serde_json::json!({"type": "at", "data": {"qq": "10001"}}),
            );
        }
        serde_json::from_value(serde_json::json!({
            "post_type": "message",
            "message_type": "group",
            "time": 1700000000,
            "self_id": 10001,
            "sub_type": "normal",
            "message_id": 43,
            "group_id": 999,
            "user_id": 123456,
            "message": segments,
            "raw_message": text,
            "font": 0,
            "sender": {"user_id": 123456, "nickname": "tester", "card": ""}
        }))
        .unwrap()
    }

    #[test]
    fn converts_private_message_to_direct_channel() {
        let names = dashmap::DashMap::new();
        let msg = QqAdapter::convert_message(&private_event("hello"), &names).expect("converted");
        assert_eq!(msg.platform, "qq");
        assert_eq!(msg.user_id, "123456");
        assert_eq!(msg.user_name, "tester");
        assert!(matches!(msg.channel, ChannelType::Direct));
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.timestamp, 1700000000);
        assert!(!msg.at_me);
    }

    #[test]
    fn converts_group_message_with_group_id() {
        let names = dashmap::DashMap::new();
        let msg =
            QqAdapter::convert_message(&group_event("hi all", false), &names).expect("converted");
        match msg.channel {
            ChannelType::Group { group_id } => assert_eq!(group_id, "999"),
            other => panic!("expected group channel, got {other:?}"),
        }
        assert!(!msg.at_me);
    }

    #[test]
    fn group_message_carries_cached_group_name() {
        let names = dashmap::DashMap::new();
        names.insert(999, "开发群".into());
        let msg = QqAdapter::convert_message(&group_event("hi", true), &names).expect("converted");
        assert_eq!(msg.group_name.as_deref(), Some("开发群"));
    }

    #[test]
    fn detects_at_me_in_group_message() {
        let names = dashmap::DashMap::new();
        let msg = QqAdapter::convert_message(&group_event("hi", true), &names).expect("converted");
        assert!(msg.at_me);
    }

    #[test]
    fn rejects_blank_content() {
        let names = dashmap::DashMap::new();
        assert!(QqAdapter::convert_message(&private_event("   "), &names).is_none());
        assert!(QqAdapter::convert_message(&private_event(""), &names).is_none());
    }

    #[test]
    fn empty_allowlist_in_allowlist_mode_returns_no_groups() {
        // Simulate the exact scenario: allowlist mode, empty allowlist.
        // get_filter_config() returns the runtime-overlaid config.
        let mut cfg = QqAdapterConfig::default();
        cfg.gate.mode = "allowlist".into();
        // allowlist is empty (default)
        let adapter = QqAdapter::new(cfg);
        // Manually set gate mode to allowlist.
        adapter.set_gate_mode(QqGateMode::Allowlist);

        let groups: Vec<(i64, String)> = vec![(111, "group a".into()), (222, "group b".into())];
        // We must test the filter logic without calling the QQ API.
        // The filter happens after the QQ API call, on the raw groups vec.
        // Create a minimal test of the filtering logic:
        let cfg = adapter.get_filter_config();
        let allow_groups: std::collections::HashSet<i64> =
            cfg.allowlist.group_ids.iter().cloned().collect();
        assert!(allow_groups.is_empty(), "allowlist is empty");
        let filtered: Vec<_> = groups
            .into_iter()
            .filter(|(gid, _)| allow_groups.contains(gid))
            .collect();
        assert!(
            filtered.is_empty(),
            "empty allowlist filters ALL groups out"
        );
    }

    #[test]
    fn rejects_non_message_events() {
        let heartbeat = serde_json::from_value(serde_json::json!({
            "post_type": "meta_event",
            "meta_event_type": "heartbeat",
            "time": 1700000000,
            "self_id": 10001,
            "interval": 5000,
            "status": {"online": true}
        }))
        .unwrap();
        let names = dashmap::DashMap::new();
        assert!(QqAdapter::convert_message(&heartbeat, &names).is_none());
    }

    #[test]
    fn converts_cq_image_segments_without_content() {
        // A message with only an image has no plain text — content empty.
        let event = serde_json::from_value(serde_json::json!({
            "post_type": "message",
            "message_type": "private",
            "time": 1700000000,
            "self_id": 10001,
            "sub_type": "friend",
            "message_id": 42,
            "user_id": 123456,
            "message": [{"type": "image", "data": {"file": "abc.png"}}],
            "raw_message": "[CQ:image,file=abc.png]",
            "font": 0,
            "sender": {"user_id": 123456, "nickname": "tester"}
        }))
        .unwrap();
        // Image-only messages carry no text, so they are dropped.
        let names = dashmap::DashMap::new();
        assert!(QqAdapter::convert_message(&event, &names).is_none());
    }
}

// ── TOML section replacer ─────────────────────────────────────────────────
