//! 插件监督：生命周期状态机（握手 / 贡献 / 调用 / 取消 / 排空 / 崩溃重启）。
//!
//! [`PluginSupervisor::start`] 启动插件并完成握手，返回可 Clone 的
//! [`PluginHandle`]；监督任务持有连接、分发插件消息，并在崩溃
//! （[`PluginConnection::recv`] 返回 [`TransportError::Closed`]、且状态非
//! Draining/Stopped）时按 [`RestartPolicy`]（默认 on-failure）自动重启。
//!
//! 关键语义：
//! - **握手**：`Hello{protocol, version, plugin_id, config}` → `Welcome` 校验
//!   （protocol 一致 / [`compatible`] / plugin_id 一致）→ `Register`（整体替换）→
//!   `Ready`；5s 内未完成 → `Err(Timeout)`，校验失败 → `Err(Protocol)` 且
//!   state=Failed。
//! - **崩溃**：在途调用全部以 `Error{code:"plugin_crashed"}` 结束 → backoff
//!   （初始 200ms、×2、上限 2s）重启，最多 5 次；重启期间 state=Pending。
//! - **调用**：`call_id = <plugin_id>:<自增序号>`；超时（`ctx.deadline_ms`，
//!   缺省 [`DEFAULT_INVOKE_TIMEOUT_MS`]）发 `Cancel` 并返回
//!   `Error{code:"timeout"}`；[`PluginHandle::cancel`] 为尽力通知（结果仍以
//!   插件回发的 `InvokeResult` 为准，`Error{code:"cancelled"}` 原样透传）。
//! - **排空**：Draining → `Drain{deadline_ms}` → 等通道关闭（或 deadline）→
//!   `connection.shutdown()` → Stopped。
//! - **热替换**（[`PluginHandle::upgrade`]）：以同一 transport + 新 config 启动
//!   新实例（握手沿用 [`HANDSHAKE_TIMEOUT`]）→ 握手成功后原子切换 `shared.conn`
//!   （invoke 自此走新实例；期间旧实例持续服务，state 保持 Ready）→ 对旧实例发
//!   `Drain{deadline_ms: timeout}` 并等其退出（或 deadline）→ 剩余时间
//!   `shutdown()` 兜底强杀；握手失败则关闭新实例、旧实例不受影响（返回 Err，
//!   可重试 / 回滚）。仅 Ready 态受理，其余状态（含崩溃重启窗口 Pending）
//!   立即 `Err(Protocol("plugin is not ready for upgrade: <state>"))`，不排队。
//!   监督循环以 `biased` 优先处理控制消息；升级与排空互斥（串行），升级期间
//!   旧连接的消息留在运输层缓冲，由后续循环迭代或旧实例排空流程照常分发
//!   （Emit / Log / 在途 InvokeResult 不丢；切换前已发给旧实例且未在排空期内
//!   应答的在途调用将等到自身超时——已知限制）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::api::{
    compatible, Cancel, Contribution, Drain, Hello, HostToPlugin, Invoke, InvokeContext,
    InvokeOutcome, LogLevel, LogRecord, PluginToHost, PROTOCOL_NAME, PROTOCOL_VERSION,
};
use crate::transport::{PluginConnection, PluginSpec, Transport, TransportError};

/// 握手总超时（Hello → Welcome → Register → Ready）。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// invoke 默认超时（`InvokeContext::deadline_ms` 为 None 时）。
pub const DEFAULT_INVOKE_TIMEOUT_MS: u64 = 600_000;

/// 控制通道容量（drain / upgrade 请求）。
const CONTROL_CAPACITY: usize = 16;

/// 热替换握手失败后，关闭新实例的宽限期（`shutdown` 尽力；超时即强杀）。
const FAILED_INSTANCE_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// 插件生命周期状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginState {
    /// 启动 / 重启中（尚未就绪）。
    Pending,
    /// 握手完成，可接受调用。
    Ready,
    /// 排空中（不再接受新调用）。
    Draining,
    /// 已停止（正常退出 / 已排空）。
    Stopped,
    /// 已失败（握手失败 / 重启预算耗尽等，附原因）。
    Failed(String),
}

/// 崩溃重启策略（默认 on-failure：backoff 200ms 起、翻倍、上限 2s、最多 5 次）。
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    /// 首次重启等待。
    pub initial_backoff: Duration,
    /// backoff 上限（翻倍后截断）。
    pub max_backoff: Duration,
    /// 最大重启尝试次数（预算耗尽 → state=Failed）。
    pub max_restarts: u32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(2),
            max_restarts: 5,
        }
    }
}

/// 插件监督者（Clone / Arc 共享；`start` 可并发启动多个插件实例）。
#[derive(Clone, Default)]
pub struct PluginSupervisor {
    policy: RestartPolicy,
}

impl PluginSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 覆盖默认重启策略。
    pub fn with_restart_policy(mut self, policy: RestartPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// 启动插件：spawn 监督任务并等待握手完成。
    ///
    /// 握手 = `Hello` → `Welcome`（校验 protocol / version / plugin_id）→
    /// `Register`（存贡献）→ `Ready`。5s 内未完成返回
    /// [`TransportError::Timeout`]；校验失败返回 [`TransportError::Protocol`]
    /// （此时插件状态为 [`PluginState::Failed`]）。
    ///
    /// 注：与 P0 骨架中的签名相比这里是 `async fn`——握手必须等待插件应答
    /// （`Transport::start` 本身也是 async），同步签名无法在返回前得到
    /// Welcome 的校验结果。
    pub async fn start(
        &self,
        spec: PluginSpec,
        transport: Box<dyn Transport>,
    ) -> Result<PluginHandle, TransportError> {
        let (ctl_tx, ctl_rx) = mpsc::channel(CONTROL_CAPACITY);
        let shared = Arc::new(Shared::new(
            spec.plugin_id.clone(),
            self.policy.clone(),
            ctl_tx,
        ));
        let (init_tx, init_rx) = oneshot::channel();
        let task_shared = shared.clone();
        tokio::spawn(async move {
            supervise(task_shared, transport, spec, ctl_rx, init_tx).await;
        });
        match init_rx.await {
            Ok(Ok(())) => Ok(PluginHandle { shared }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(TransportError::Io(
                "supervisor task ended before handshake completed".to_string(),
            )),
        }
    }
}

/// 单个插件实例的句柄（Clone；与监督任务共享状态）。
#[derive(Clone)]
pub struct PluginHandle {
    shared: Arc<Shared>,
}

impl PluginHandle {
    /// 插件实例 id。
    pub fn plugin_id(&self) -> &str {
        &self.shared.plugin_id
    }

    /// 当前状态快照。
    pub fn state(&self) -> PluginState {
        self.shared.state()
    }

    /// 最近一次 `Register` 的贡献集合（整体替换语义）。
    pub fn contributions(&self) -> Vec<Contribution> {
        self.shared.contributions()
    }

    /// 已发生的重启次数（按重启尝试计数，含成功与失败尝试）。
    pub fn restarts(&self) -> u32 {
        self.shared.restarts.load(Ordering::SeqCst)
    }

    /// 自最近一次启动（[`PluginSupervisor::start`]）以来完成的热替换次数。
    ///
    /// 仅 [`PluginHandle::upgrade`] 成功原子切换后 +1；崩溃自动重启不计入
    /// （见 [`PluginHandle::restarts`]）。
    pub fn hot_replaces(&self) -> u32 {
        self.shared.hot_replaces.load(Ordering::SeqCst)
    }

    /// 排空：state=Draining → 发 `Drain{deadline_ms}` → 等待插件通道关闭
    /// （或 deadline）→ `connection.shutdown()` → state=Stopped。
    ///
    /// 已 Stopped / Failed（监督任务已退出）时直接 `Ok(())`（幂等）。
    /// 若 shutdown 超时，仍置 Stopped 并返回 `Err(Timeout)`。
    pub async fn drain(&self, deadline: Duration) -> Result<(), TransportError> {
        match self.shared.state() {
            PluginState::Stopped | PluginState::Failed(_) => return Ok(()),
            _ => {}
        }
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .shared
            .ctl
            .send(Control::Drain {
                deadline,
                ack: ack_tx,
            })
            .await
            .is_err()
        {
            // 监督任务已退出（正常路径：已 Stopped / Failed）：视为完成。
            if !matches!(
                self.shared.state(),
                PluginState::Failed(_) | PluginState::Stopped
            ) {
                self.shared.set_state(PluginState::Stopped);
            }
            return Ok(());
        }
        match ack_rx.await {
            Ok(res) => res,
            // 监督任务在应答前退出（不应发生）：按已停止处理。
            Err(_) => Ok(()),
        }
    }

    /// 热替换：用新配置启动一个新进程实例，握手成功后原子切换流量，
    /// 然后优雅排空旧实例；失败则保留旧实例继续服务（可回滚）。
    ///
    /// 语义：
    /// - 仅 [`PluginState::Ready`] 受理；Stopped / Failed / Draining / Pending
    ///   （含崩溃重启窗口）立即返回
    ///   `Err(Protocol("plugin is not ready for upgrade: <state>"))`，**不排队**；
    /// - 新实例经同一 transport 以 `PluginSpec { plugin_id: 原 id, config }`
    ///   启动，握手沿用 5s 上限；此间旧实例持续服务（state 保持 Ready）；
    /// - 握手失败 → 关闭新实例（`shutdown` 尽力）、旧实例与贡献集合不受影响，
    ///   返回 Err（可重试 / 回滚）；
    /// - 握手成功 → 原子切换（此后 `invoke` 走新实例）→ 对旧实例发
    ///   `Drain{deadline_ms: timeout}` 并等其退出（或 deadline）→
    ///   `shutdown` 兜底强杀；返回 Ok 前旧实例已被排空 / 强杀。
    ///   旧实例排空期间的 Emit / Log / 在途 InvokeResult 仍照常分发；
    ///   切换前已发给旧实例、且旧实例未在排空期内应答的在途调用不在本次
    ///   升级的等待范围内（各自按调用超时兜底——已知限制）。
    /// - 成功 +1 [`PluginHandle::hot_replaces`]；旧实例强杀兜底不影响成功结果。
    ///
    /// `timeout` 是旧实例的优雅排空期限（`Drain.deadline_ms` 与其后兜底
    /// `shutdown` 的预算）；新实例握手使用固定 [`HANDSHAKE_TIMEOUT`]。
    /// 监督任务已退出（如并发 drain 先完成）时返回非就绪错误。
    /// 并发 `upgrade` / `drain` 在监督循环内串行执行，按到达顺序处理。
    pub async fn upgrade(&self, config: Value, timeout: Duration) -> Result<(), TransportError> {
        let state = self.shared.state();
        if state != PluginState::Ready {
            return Err(not_ready_error(&state));
        }
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .shared
            .ctl
            .send(Control::Upgrade {
                config,
                timeout,
                ack: ack_tx,
            })
            .await
            .is_err()
        {
            return Err(self.upgrade_unavailable_error());
        }
        match ack_rx.await {
            Ok(res) => res,
            Err(_) => Err(self.upgrade_unavailable_error()),
        }
    }

    /// 监督任务已退出 / 竞态下的非就绪错误（[`PluginHandle::upgrade`] 用）。
    fn upgrade_unavailable_error(&self) -> TransportError {
        let state = self.shared.state();
        if state == PluginState::Ready {
            TransportError::Io("supervisor task ended before upgrade completed".to_string())
        } else {
            not_ready_error(&state)
        }
    }

    /// 调用一个贡献。
    ///
    /// - `call_id = <plugin_id>:<自增序号>`（从 1 开始），登记在途表等待
    ///   `InvokeResult`；
    /// - 超时 = `ctx.deadline_ms` 或默认 [`DEFAULT_INVOKE_TIMEOUT_MS`]；超时后
    ///   发送 `Cancel` 并返回 `Error{code:"timeout"}`；
    /// - 插件回发的任何结果（含 `Error{code:"cancelled"}`）原样返回；
    /// - 插件不可用时的错误码：`stopped` / `failed` / `draining` / `not_ready` /
    ///   `plugin_crashed` / `transport_error`。
    pub async fn invoke(
        &self,
        contribution: &str,
        ctx: InvokeContext,
        payload: Value,
    ) -> InvokeOutcome {
        self.shared.invoke(contribution, ctx, payload).await
    }

    /// 取消在途调用：在途表标记 + 尽力发送 `Cancel`（fire-and-forget）。
    ///
    /// 调用结果仍以插件回发的 `InvokeResult` 为准（`cancelled` 原样透传）；
    /// 插件无响应时由 [`invoke`](PluginHandle::invoke) 的超时兜底。
    pub fn cancel(&self, call_id: &str) {
        self.shared.cancel(call_id);
    }

    /// 取走累计的插件事件（`Emit`），返回后队列清空。
    pub fn take_events(&self) -> Vec<(String, Value)> {
        self.shared.take_events()
    }
}

/// 监督任务与句柄共享的状态。
struct Shared {
    plugin_id: String,
    policy: RestartPolicy,
    state: Mutex<PluginState>,
    contributions: Mutex<Vec<Contribution>>,
    restarts: AtomicU32,
    /// 完成的热替换次数（仅成功原子切换计数；崩溃重启不计入）。
    hot_replaces: AtomicU32,
    /// 调用序号（`call_id` 后缀，从 1 开始）。
    seq: AtomicU64,
    pending: Mutex<HashMap<String, PendingCall>>,
    events: Mutex<Vec<(String, Value)>>,
    /// 当前连接（None = 未启动 / 重启中 / 已停）。
    conn: Mutex<Option<Arc<dyn PluginConnection>>>,
    /// 控制通道（drain / upgrade 请求）。
    ctl: mpsc::Sender<Control>,
}

/// 在途调用。
struct PendingCall {
    result_tx: oneshot::Sender<InvokeOutcome>,
    /// 是否收到过 [`PluginHandle::cancel`] 标记（仅记录；结果仍以插件回发为准）。
    cancel_requested: bool,
}

/// 监督控制命令。
enum Control {
    /// 排空请求；`ack` 携带最终结果。
    Drain {
        deadline: Duration,
        ack: oneshot::Sender<Result<(), TransportError>>,
    },
    /// 热替换请求；`ack` 携带最终结果（见 [`PluginHandle::upgrade`]）。
    Upgrade {
        config: Value,
        timeout: Duration,
        ack: oneshot::Sender<Result<(), TransportError>>,
    },
}

/// `select!` 输出（避免在分支 future 存活期间借用 `ctl_rx`）。
enum Step {
    Msg(Result<PluginToHost, TransportError>),
    Ctl(Option<Control>),
}

/// 重启流程结果。
enum Restart {
    /// 重启成功，新连接已就绪。
    Restarted,
    /// 重启等待期间收到排空请求并已完成。
    Drained,
    /// 放弃（预算耗尽 / 控制端消失）。
    Dead,
}

impl Shared {
    fn new(plugin_id: String, policy: RestartPolicy, ctl: mpsc::Sender<Control>) -> Self {
        Self {
            plugin_id,
            policy,
            state: Mutex::new(PluginState::Pending),
            contributions: Mutex::new(Vec::new()),
            restarts: AtomicU32::new(0),
            hot_replaces: AtomicU32::new(0),
            seq: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            conn: Mutex::new(None),
            ctl,
        }
    }

    fn state(&self) -> PluginState {
        self.state.lock().expect("state lock poisoned").clone()
    }

    fn set_state(&self, state: PluginState) {
        *self.state.lock().expect("state lock poisoned") = state;
    }

    fn contributions(&self) -> Vec<Contribution> {
        self.contributions
            .lock()
            .expect("contributions lock poisoned")
            .clone()
    }

    fn set_contributions(&self, list: Vec<Contribution>) {
        tracing::info!(plugin = %self.plugin_id, count = list.len(), "plugin registered contributions");
        *self
            .contributions
            .lock()
            .expect("contributions lock poisoned") = list;
    }

    fn push_event(&self, event: String, payload: Value) {
        self.events
            .lock()
            .expect("events lock poisoned")
            .push((event, payload));
    }

    fn take_events(&self) -> Vec<(String, Value)> {
        std::mem::take(&mut *self.events.lock().expect("events lock poisoned"))
    }

    fn current_conn(&self) -> Option<Arc<dyn PluginConnection>> {
        self.conn.lock().expect("conn lock poisoned").clone()
    }

    fn set_conn(&self, conn: Option<Arc<dyn PluginConnection>>) {
        *self.conn.lock().expect("conn lock poisoned") = conn;
    }

    fn insert_pending(&self, call_id: String, call: PendingCall) {
        self.pending
            .lock()
            .expect("pending lock poisoned")
            .insert(call_id, call);
    }

    fn remove_pending(&self, call_id: &str) -> Option<PendingCall> {
        self.pending
            .lock()
            .expect("pending lock poisoned")
            .remove(call_id)
    }

    /// 崩溃：全部在途调用以 `plugin_crashed` 结束。
    fn fail_pending_calls(&self, reason: &str) {
        let drained: Vec<(String, PendingCall)> = self
            .pending
            .lock()
            .expect("pending lock poisoned")
            .drain()
            .collect();
        for (call_id, call) in drained {
            let _ = call.result_tx.send(InvokeOutcome::Error {
                code: "plugin_crashed".to_string(),
                message: format!("{reason} while call {call_id} was in flight"),
            });
        }
    }

    async fn invoke(
        &self,
        contribution: &str,
        ctx: InvokeContext,
        payload: Value,
    ) -> InvokeOutcome {
        match self.state() {
            PluginState::Stopped => return outcome_error("stopped", "plugin is stopped"),
            PluginState::Failed(reason) => {
                return outcome_error("failed", format!("plugin failed: {reason}"))
            }
            PluginState::Draining => return outcome_error("draining", "plugin is draining"),
            _ => {}
        }
        let Some(conn) = self.current_conn() else {
            return outcome_error("not_ready", "plugin is not ready (starting or restarting)");
        };

        let call_id = format!(
            "{}:{}",
            self.plugin_id,
            self.seq.fetch_add(1, Ordering::SeqCst) + 1
        );
        let (result_tx, result_rx) = oneshot::channel();
        self.insert_pending(
            call_id.clone(),
            PendingCall {
                result_tx,
                cancel_requested: false,
            },
        );

        let msg = HostToPlugin::Invoke(Invoke {
            call_id: call_id.clone(),
            contribution: contribution.to_string(),
            ctx: ctx.clone(),
            payload,
        });
        if let Err(e) = conn.send(msg).await {
            self.remove_pending(&call_id);
            let code = if matches!(e, TransportError::Closed) {
                "plugin_crashed"
            } else {
                "transport_error"
            };
            return outcome_error(code, format!("failed to send invoke: {e}"));
        }

        let deadline_ms = ctx.deadline_ms.unwrap_or(DEFAULT_INVOKE_TIMEOUT_MS);
        match tokio::time::timeout(Duration::from_millis(deadline_ms), result_rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => {
                // 结果通道被提前丢弃（不应发生；防御性处理）。
                self.remove_pending(&call_id);
                outcome_error("plugin_crashed", "invoke result channel closed")
            }
            Err(_) => {
                // 超时：先摘除在途表，再尽力通知对端取消。
                self.remove_pending(&call_id);
                if let Err(e) = conn
                    .send(HostToPlugin::Cancel(Cancel {
                        call_id: call_id.clone(),
                    }))
                    .await
                {
                    tracing::debug!(plugin = %self.plugin_id, call_id = %call_id, error = %e, "failed to deliver timeout cancel");
                }
                outcome_error("timeout", format!("invoke timed out after {deadline_ms}ms"))
            }
        }
    }

    fn cancel(&self, call_id: &str) {
        let marked = match self
            .pending
            .lock()
            .expect("pending lock poisoned")
            .get_mut(call_id)
        {
            Some(call) => {
                call.cancel_requested = true;
                true
            }
            None => false,
        };
        if !marked {
            tracing::debug!(plugin = %self.plugin_id, call_id = %call_id, "cancel for unknown/finished call");
        }
        if let Some(conn) = self.current_conn() {
            let call_id = call_id.to_string();
            let plugin_id = self.plugin_id.clone();
            // fire-and-forget：尽力送达（结果仍以插件回发为准）。
            tokio::spawn(async move {
                if let Err(e) = conn
                    .send(HostToPlugin::Cancel(Cancel {
                        call_id: call_id.clone(),
                    }))
                    .await
                {
                    tracing::debug!(plugin = %plugin_id, call_id = %call_id, error = %e, "failed to deliver cancel");
                }
            });
        }
    }
}

fn outcome_error(code: &str, message: impl Into<String>) -> InvokeOutcome {
    InvokeOutcome::Error {
        code: code.to_string(),
        message: message.into(),
    }
}

/// 监督任务主体：握手 → 消息循环（含崩溃重启 / 排空 / 热替换）。
///
/// 循环每轮从 `shared` 取**当前**连接再 `select!`（`biased`：控制消息优先），
/// 因此热替换切换连接后自动观察新连接。升级 / 排空在循环内串行执行（互斥）：
/// 升级执行期间（含旧实例排空）不处理其它控制请求，旧连接的消息留在运输层
/// 缓冲（stdio 管道 / inproc 通道），由后续循环迭代或旧实例排空流程分发，
/// 不丢消息。
async fn supervise(
    shared: Arc<Shared>,
    transport: Box<dyn Transport>,
    spec: PluginSpec,
    mut ctl_rx: mpsc::Receiver<Control>,
    init_tx: oneshot::Sender<Result<(), TransportError>>,
) {
    match start_and_handshake(&shared, transport.as_ref(), &spec).await {
        Ok((conn, contributions)) => {
            if let Some(list) = contributions {
                shared.set_contributions(list);
            }
            shared.set_conn(Some(conn));
            shared.set_state(PluginState::Ready);
            tracing::info!(plugin = %shared.plugin_id, "plugin ready");
            let _ = init_tx.send(Ok(()));
        }
        Err(e) => {
            tracing::error!(plugin = %shared.plugin_id, error = %e, "plugin handshake failed");
            shared.set_state(PluginState::Failed(e.to_string()));
            let _ = init_tx.send(Err(e));
            return;
        }
    }

    loop {
        let Some(conn) = shared.current_conn() else {
            // 不应发生（Ready 态必有连接）；防御性收尾。
            shared.set_state(PluginState::Stopped);
            return;
        };
        let step = tokio::select! {
            // 控制消息优先（`biased`）：Drain / Upgrade 先于连接消息处理；
            // 尚未处理的连接消息留在运输层缓冲，由后续循环迭代或排空流程
            // 分发，不会丢失。
            biased;
            ctl = ctl_rx.recv() => Step::Ctl(ctl),
            msg = conn.recv() => Step::Msg(msg),
        };
        match step {
            Step::Msg(Ok(msg)) => dispatch(&shared, msg),
            Step::Msg(Err(e)) => {
                tracing::warn!(plugin = %shared.plugin_id, error = %e, "plugin connection lost; treating as crash");
                shared.fail_pending_calls("plugin connection lost");
                match restart_after_crash(&shared, transport.as_ref(), &spec, &mut ctl_rx).await {
                    Restart::Restarted => {}
                    Restart::Drained | Restart::Dead => return,
                }
            }
            Step::Ctl(Some(Control::Drain { deadline, ack })) => {
                shared.set_state(PluginState::Draining);
                let res = drain_connection(&shared, deadline).await;
                shared.set_conn(None);
                shared.set_state(PluginState::Stopped);
                tracing::info!(plugin = %shared.plugin_id, "plugin stopped (drained)");
                let _ = ack.send(res);
                return;
            }
            Step::Ctl(Some(Control::Upgrade {
                config,
                timeout,
                ack,
            })) => {
                let res =
                    upgrade_instance(&shared, transport.as_ref(), &spec, config, timeout).await;
                let _ = ack.send(res);
            }
            Step::Ctl(None) => {
                // 控制端全部消失（防御性分支）：静默排空后退出。
                shared.set_state(PluginState::Draining);
                let _ = drain_connection(&shared, Duration::from_millis(500)).await;
                shared.set_conn(None);
                shared.set_state(PluginState::Stopped);
                return;
            }
        }
    }
}

/// 启动 + 握手（首次启动与崩溃重启共用）。
///
/// 返回 `(连接, 新实例注册的贡献)`；`Register` 未到达时贡献为 `None`
/// （调用方保留现有集合）。调用方负责在此之前把状态置为
/// [`PluginState::Pending`]（首次启动由 [`Shared::new`] 置位；重启由
/// `restart_after_crash` 置位）。
async fn start_and_handshake(
    shared: &Shared,
    transport: &dyn Transport,
    spec: &PluginSpec,
) -> Result<(Arc<dyn PluginConnection>, Option<Vec<Contribution>>), TransportError> {
    let conn: Arc<dyn PluginConnection> = Arc::from(transport.start(spec.clone()).await?);
    let registered = run_handshake(shared, &conn, spec).await?;
    Ok((conn, registered))
}

/// 握手（Hello → Welcome → Register → Ready），总超时 [`HANDSHAKE_TIMEOUT`]。
///
/// 成功返回新实例注册的贡献（`Register` 未到达时为 `None`），**不**写入
/// `shared`——由调用方在握手成功后应用（热替换失败时须保持旧贡献集合）。
/// 握手期间的 `Emit` / `Log` 照常分发。
async fn run_handshake(
    shared: &Shared,
    conn: &Arc<dyn PluginConnection>,
    spec: &PluginSpec,
) -> Result<Option<Vec<Contribution>>, TransportError> {
    let plugin_id = shared.plugin_id.clone();

    let handshake = async {
        conn.send(HostToPlugin::Hello(Hello {
            protocol: PROTOCOL_NAME.to_string(),
            version: PROTOCOL_VERSION,
            plugin_id: plugin_id.clone(),
            config: spec.config.clone(),
        }))
        .await?;
        let mut welcomed = false;
        let mut registered: Option<Vec<Contribution>> = None;
        loop {
            match conn.recv().await? {
                PluginToHost::Welcome(w) => {
                    if w.protocol != PROTOCOL_NAME {
                        return Err(TransportError::Protocol(format!(
                            "protocol mismatch: expected {PROTOCOL_NAME:?}, got {:?}",
                            w.protocol
                        )));
                    }
                    if !compatible(PROTOCOL_VERSION, w.version) {
                        return Err(TransportError::Protocol(format!(
                            "incompatible protocol version: host {PROTOCOL_VERSION}, plugin {}",
                            w.version
                        )));
                    }
                    if w.plugin_id != plugin_id {
                        return Err(TransportError::Protocol(format!(
                            "plugin id mismatch: expected {plugin_id:?}, got {:?}",
                            w.plugin_id
                        )));
                    }
                    welcomed = true;
                    tracing::debug!(plugin = %plugin_id, version = w.version, capabilities = ?w.capabilities, "plugin welcomed");
                }
                PluginToHost::Register(r) => registered = Some(r.contributions),
                PluginToHost::Ready => {
                    if !welcomed {
                        return Err(TransportError::Protocol(
                            "plugin sent Ready before Welcome".to_string(),
                        ));
                    }
                    return Ok(registered);
                }
                PluginToHost::Failed(f) => {
                    return Err(TransportError::Protocol(format!(
                        "plugin init failed: {} ({})",
                        f.message, f.code
                    )));
                }
                PluginToHost::Emit(e) => shared.push_event(e.event, e.payload),
                PluginToHost::Log(l) => log_record(&plugin_id, &l),
                PluginToHost::InvokeResult(r) => {
                    tracing::debug!(plugin = %plugin_id, call_id = %r.call_id, "ignoring InvokeResult during handshake");
                }
            }
        }
    };

    match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
        Ok(res) => res,
        Err(_) => Err(TransportError::Timeout),
    }
}

/// 崩溃后的重启循环（backoff 等待可被排空请求打断）。
async fn restart_after_crash(
    shared: &Shared,
    transport: &dyn Transport,
    spec: &PluginSpec,
    ctl_rx: &mut mpsc::Receiver<Control>,
) -> Restart {
    let mut backoff = shared.policy.initial_backoff;
    loop {
        let used = shared.restarts.load(Ordering::SeqCst);
        if used >= shared.policy.max_restarts {
            tracing::error!(plugin = %shared.plugin_id, used, "restart budget exhausted; giving up");
            shared.set_state(PluginState::Failed(format!(
                "restart budget exhausted ({used}/{} restarts) after crash",
                shared.policy.max_restarts
            )));
            return Restart::Dead;
        }
        shared.set_state(PluginState::Pending);
        let attempt = used + 1;
        tracing::warn!(
            plugin = %shared.plugin_id,
            attempt,
            max = shared.policy.max_restarts,
            backoff_ms = backoff.as_millis() as u64,
            "plugin crashed; scheduling restart"
        );

        // backoff（可被 Drain 打断）。每个分支都直接离开本次等待：
        // sleep 完成 → 继续外层重试循环；Drain/通道关闭 → 返回终态。
        if !backoff.is_zero() {
            let sleep = tokio::time::sleep(backoff);
            tokio::pin!(sleep);
            tokio::select! {
                _ = &mut sleep => {}
                cmd = ctl_rx.recv() => match cmd {
                    Some(Control::Drain { deadline, ack }) => {
                        shared.set_state(PluginState::Draining);
                        let res = drain_connection(shared, deadline).await;
                        shared.set_conn(None);
                        shared.set_state(PluginState::Stopped);
                        let _ = ack.send(res);
                        return Restart::Drained;
                    }
                    Some(Control::Upgrade { ack, .. }) => {
                        // 重启窗口（Pending）内的升级请求：按「非就绪拒绝」
                        // 语义直接回报错误，不排队等待重启完成。
                        let _ = ack.send(Err(not_ready_error(&shared.state())));
                    }
                    None => {
                        shared.set_state(PluginState::Stopped);
                        return Restart::Dead;
                    }
                },
            }
        }

        shared.restarts.fetch_add(1, Ordering::SeqCst);
        match start_and_handshake(shared, transport, spec).await {
            Ok((conn, contributions)) => {
                if let Some(list) = contributions {
                    shared.set_contributions(list);
                }
                shared.set_conn(Some(conn));
                shared.set_state(PluginState::Ready);
                tracing::info!(plugin = %shared.plugin_id, attempt, "plugin restarted");
                return Restart::Restarted;
            }
            Err(e) => {
                tracing::warn!(plugin = %shared.plugin_id, attempt, error = %e, "restart attempt failed");
                backoff = (backoff * 2).min(shared.policy.max_backoff);
            }
        }
    }
}

/// 排空：发 Drain → 等通道关闭（或 deadline）→ shutdown。
async fn drain_connection(shared: &Shared, deadline: Duration) -> Result<(), TransportError> {
    let Some(conn) = shared.current_conn() else {
        return Ok(()); // 重启中等：无连接可排空
    };
    let started = tokio::time::Instant::now();
    shared.set_state(PluginState::Draining);

    if let Err(e) = conn
        .send(HostToPlugin::Drain(Drain {
            deadline_ms: deadline.as_millis() as u64,
        }))
        .await
    {
        tracing::debug!(plugin = %shared.plugin_id, error = %e, "failed to deliver Drain");
    }

    // 等待插件主动退出（通道关闭），期间照常分发消息（在途结果 / 事件 / 日志）。
    let wait = async {
        while let Ok(msg) = conn.recv().await {
            dispatch(shared, msg);
        }
    };
    let remaining = deadline.saturating_sub(started.elapsed());
    let _ = tokio::time::timeout(remaining, wait).await;

    let remaining = deadline
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(1));
    let res = conn.shutdown(remaining).await;
    shared.set_conn(None);
    res
}

/// 热替换核心（在监督任务内串行执行，独占监督循环——升级与排空互斥）。
///
/// 流程见 [`PluginHandle::upgrade`] 文档；要点：
/// - 仅 Ready 态受理，其余状态返回 `Protocol("plugin is not ready for upgrade: …")`；
/// - 新实例握手期间 state 保持 Ready（旧实例继续服务）；
/// - 握手失败：关闭新实例（`shutdown` 尽力）、旧实例与贡献集合均不受影响；
/// - 握手成功：原子切换 `shared.conn`（invoke 自此走新实例）→ 应用新注册的
///   贡献 → `hot_replaces` +1 → 排空旧实例
///   （`Drain{deadline_ms: timeout}` → 等其退出 → `shutdown` 兜底强杀）。
async fn upgrade_instance(
    shared: &Shared,
    transport: &dyn Transport,
    spec: &PluginSpec,
    config: Value,
    timeout: Duration,
) -> Result<(), TransportError> {
    let state = shared.state();
    if state != PluginState::Ready {
        return Err(not_ready_error(&state));
    }
    let Some(old_conn) = shared.current_conn() else {
        // 防御性：Ready 态必有连接。
        return Err(not_ready_error(&PluginState::Pending));
    };

    let new_spec = PluginSpec {
        plugin_id: spec.plugin_id.clone(),
        config,
    };
    tracing::info!(plugin = %shared.plugin_id, "upgrade: starting replacement instance");
    let new_conn: Arc<dyn PluginConnection> = match transport.start(new_spec.clone()).await {
        Ok(conn) => Arc::from(conn),
        Err(e) => {
            tracing::warn!(
                plugin = %shared.plugin_id,
                error = %e,
                "upgrade: failed to start replacement; keeping current instance"
            );
            return Err(e);
        }
    };
    let contributions = match run_handshake(shared, &new_conn, &new_spec).await {
        Ok(registered) => registered,
        Err(e) => {
            tracing::warn!(
                plugin = %shared.plugin_id,
                error = %e,
                "upgrade: replacement handshake failed; keeping current instance"
            );
            // 关闭新实例（尽力；超时即强杀），旧实例不受影响。
            if new_conn
                .shutdown(FAILED_INSTANCE_SHUTDOWN_GRACE)
                .await
                .is_err()
            {
                tracing::warn!(
                    plugin = %shared.plugin_id,
                    "upgrade: replacement instance did not exit gracefully after failed handshake"
                );
            }
            return Err(e);
        }
    };

    // 原子切换：invoke 自此走新实例；贡献集合同步为新实例注册
    // （新实例未发 Register 时保留现有集合）。
    shared.set_conn(Some(new_conn));
    if let Some(list) = contributions {
        shared.set_contributions(list);
    }
    shared.hot_replaces.fetch_add(1, Ordering::SeqCst);
    tracing::info!(plugin = %shared.plugin_id, "upgrade: traffic switched to replacement instance");

    drain_retired(shared, &old_conn, timeout).await;
    Ok(())
}

/// 排空被热替换下来的旧实例：`Drain{deadline_ms: timeout}` → 等其通道关闭
/// （或 deadline；期间照常分发 Emit / Log / 在途 InvokeResult）→
/// `shutdown(剩余时间)` 兜底强杀。
///
/// 旧实例的退出结果（优雅 / 强杀）不影响热替换结果：流量已在新实例上。
async fn drain_retired(shared: &Shared, conn: &Arc<dyn PluginConnection>, timeout: Duration) {
    let started = tokio::time::Instant::now();
    if let Err(e) = conn
        .send(HostToPlugin::Drain(Drain {
            deadline_ms: timeout.as_millis() as u64,
        }))
        .await
    {
        tracing::debug!(
            plugin = %shared.plugin_id,
            error = %e,
            "upgrade: failed to deliver Drain to retired instance"
        );
    }

    let wait = async {
        while let Ok(msg) = conn.recv().await {
            dispatch(shared, msg);
        }
    };
    let remaining = timeout.saturating_sub(started.elapsed());
    let _ = tokio::time::timeout(remaining, wait).await;

    let remaining = timeout
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(1));
    match conn.shutdown(remaining).await {
        Ok(()) => tracing::info!(
            plugin = %shared.plugin_id,
            "upgrade: retired instance drained and exited"
        ),
        Err(e) => tracing::warn!(
            plugin = %shared.plugin_id,
            error = %e,
            "upgrade: retired instance killed after drain deadline"
        ),
    }
}

/// 状态短标签（错误消息用）。
fn state_label(state: &PluginState) -> String {
    match state {
        PluginState::Pending => "pending".to_string(),
        PluginState::Ready => "ready".to_string(),
        PluginState::Draining => "draining".to_string(),
        PluginState::Stopped => "stopped".to_string(),
        PluginState::Failed(reason) => format!("failed({reason})"),
    }
}

/// 「插件非 Ready，无法升级」错误（不排队语义）。
fn not_ready_error(state: &PluginState) -> TransportError {
    TransportError::Protocol(format!(
        "plugin is not ready for upgrade: {}",
        state_label(state)
    ))
}

/// 分发插件 → 宿主消息。
fn dispatch(shared: &Shared, msg: PluginToHost) {
    match msg {
        PluginToHost::InvokeResult(r) => match shared.remove_pending(&r.call_id) {
            Some(call) => {
                if call.cancel_requested {
                    tracing::debug!(plugin = %shared.plugin_id, call_id = %r.call_id, "resolving call marked cancelled");
                }
                // 结果原样透传（含 Error{code:"cancelled"}）。
                let _ = call.result_tx.send(r.outcome);
            }
            None => tracing::debug!(
                plugin = %shared.plugin_id,
                call_id = %r.call_id,
                "ignoring InvokeResult for unknown/finished call"
            ),
        },
        PluginToHost::Emit(e) => {
            tracing::debug!(plugin = %shared.plugin_id, event = %e.event, "plugin event");
            shared.push_event(e.event, e.payload);
        }
        PluginToHost::Log(l) => log_record(&shared.plugin_id, &l),
        PluginToHost::Register(r) => shared.set_contributions(r.contributions),
        PluginToHost::Welcome(w) => tracing::warn!(
            plugin = %shared.plugin_id,
            version = w.version,
            "unexpected Welcome after handshake; ignored"
        ),
        PluginToHost::Ready => {
            tracing::debug!(plugin = %shared.plugin_id, "plugin re-sent Ready")
        }
        PluginToHost::Failed(f) => tracing::warn!(
            plugin = %shared.plugin_id,
            code = %f.code,
            message = %f.message,
            "plugin reported failure"
        ),
    }
}

/// 日志转发到 tracing（带 `plugin = <id>` 前缀）。
fn log_record(plugin_id: &str, rec: &LogRecord) {
    let fields = rec.fields.as_ref();
    match rec.level {
        LogLevel::Trace => {
            tracing::trace!(plugin = %plugin_id, fields = ?fields, "{}", rec.message)
        }
        LogLevel::Debug => {
            tracing::debug!(plugin = %plugin_id, fields = ?fields, "{}", rec.message)
        }
        LogLevel::Info => tracing::info!(plugin = %plugin_id, fields = ?fields, "{}", rec.message),
        LogLevel::Warn => tracing::warn!(plugin = %plugin_id, fields = ?fields, "{}", rec.message),
        LogLevel::Error => {
            tracing::error!(plugin = %plugin_id, fields = ?fields, "{}", rec.message)
        }
    }
}
