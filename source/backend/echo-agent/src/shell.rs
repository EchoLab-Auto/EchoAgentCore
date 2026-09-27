//! 后台 shell 会话 —— 持久的 bash 进程，供面板/LLM 反复执行命令。
//!
//! 设计要点：
//! - 每个会话 = 一个 `bash --noprofile --norc` 子进程（stdin/stdout/stderr 管道）
//! - 命令执行：写命令 + 随机哨兵标记，读 stdout/stderr 直到哨兵出现（或超时）
//! - 会话串行执行（一次一条命令）；不同会话并行；上限 [`MAX_SHELL_SESSIONS`]
//! - 事件通过注入的 emit 回调广播（组合根接到默认 agent 的 handle），
//!   Panel 据此做终端可视化（流式输出）
//! - 用户可 `ShellStop` 销毁；进程意外退出时自动清理并广播关闭事件

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::event::ShellSessionInfo;

pub const MAX_SHELL_SESSIONS: usize = 8;
/// 单条命令默认超时（秒）。
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 120;

/// 广播给面板的事件载荷（由 [`shell_event_to_backend`] 翻译成 BackendEvent）。
pub enum ShellEvent {
    Started(ShellSessionInfo),
    ExecStarted {
        session_id: String,
        seq: u64,
        command: String,
    },
    Output {
        session_id: String,
        seq: u64,
        chunk: String,
    },
    Done {
        session_id: String,
        seq: u64,
        success: bool,
        elapsed_ms: u64,
    },
    Closed {
        session_id: String,
        reason: String,
    },
}

pub type ShellEmit = Arc<dyn Fn(ShellEvent) + Send + Sync>;

// ── 工具执行期的归属 team ──
// LLM 的 shell_start 工具经进程级 manager 启动会话，工具本身拿不到 agent
// 上下文；`Agent::run_tool` 在执行前把本 persona 的 team_id 写入线程本地，
// 工具读取它给新会话打标（面板按当前 agent 过滤）。
thread_local! {
    static TOOL_TEAM_ID: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

pub fn set_tool_team_id(team_id: Option<String>) {
    TOOL_TEAM_ID.with(|slot| *slot.borrow_mut() = team_id);
}

pub fn current_tool_team_id() -> Option<String> {
    TOOL_TEAM_ID.with(|slot| slot.borrow().clone())
}

struct ShellSession {
    session_id: String,
    /// 归属 team（persona）：面板按当前 agent 过滤；None = 进程级（旧路径）。
    team_id: Option<String>,
    workdir: String,
    created_at_ms: i64,
    last_active_ms: std::sync::atomic::AtomicI64,
    exec_count: std::sync::atomic::AtomicU64,
    /// 子进程句柄（kill/try_wait 需要 &mut self，用 tokio Mutex）。
    child: tokio::sync::Mutex<Child>,
    stdin: tokio::sync::Mutex<ChildStdin>,
    stdout: tokio::sync::Mutex<BufReader<ChildStdout>>,
    stderr: tokio::sync::Mutex<BufReader<ChildStderr>>,
    /// 串行执行锁：一次只跑一条命令。
    exec_lock: tokio::sync::Mutex<()>,
}

impl ShellSession {
    fn info(&self) -> ShellSessionInfo {
        ShellSessionInfo {
            session_id: self.session_id.clone(),
            team_id: self.team_id.clone(),
            workdir: self.workdir.clone(),
            created_at_ms: self.created_at_ms,
            last_active_ms: self
                .last_active_ms
                .load(std::sync::atomic::Ordering::Relaxed),
            exec_count: self.exec_count.load(std::sync::atomic::Ordering::Relaxed),
            running: true,
            last_output: None,
        }
    }
}

#[derive(Default)]
pub struct ShellManager {
    sessions: std::sync::Mutex<HashMap<String, Arc<ShellSession>>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl ShellManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 列出会话；`team_id` 为 Some 时只返回该 persona 的会话
    /// （None = 全部，兼容进程级旧路径）。
    pub fn list(&self, team_id: Option<&str>) -> Vec<ShellSessionInfo> {
        let sessions = self.sessions.lock().unwrap();
        let mut list: Vec<ShellSessionInfo> = sessions
            .values()
            .filter(|s| team_id.is_none() || s.team_id.as_deref() == team_id)
            .map(|s| ShellSessionInfo {
                session_id: s.session_id.clone(),
                team_id: s.team_id.clone(),
                workdir: s.workdir.clone(),
                created_at_ms: s.created_at_ms,
                last_active_ms: s.last_active_ms.load(std::sync::atomic::Ordering::Relaxed),
                exec_count: s.exec_count.load(std::sync::atomic::Ordering::Relaxed),
                running: true,
                last_output: None,
            })
            .collect();
        list.sort_by_key(|a| a.created_at_ms);
        list
    }

    /// 启动一个新会话；超过上限时报错。
    pub async fn start(
        &self,
        workdir: Option<String>,
        team_id: Option<String>,
        emit: &ShellEmit,
    ) -> Result<ShellSessionInfo, String> {
        let dir = workdir.filter(|d| !d.trim().is_empty()).unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| ".".into())
                .display()
                .to_string()
        });
        {
            let sessions = self.sessions.lock().unwrap();
            if sessions.len() >= MAX_SHELL_SESSIONS {
                return Err(format!(
                    "shell session limit reached ({MAX_SHELL_SESSIONS}); stop one first"
                ));
            }
        }
        let mut child = Command::new("bash")
            .arg("--noprofile")
            .arg("--norc")
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn bash failed: {e}"))?;
        let stdin = child.stdin.take().ok_or("bash stdin unavailable")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("bash stdout unavailable")?);
        let stderr = BufReader::new(child.stderr.take().ok_or("bash stderr unavailable")?);
        let seq = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let session_id = format!("sh-{seq}");
        let now = chrono::Utc::now().timestamp_millis();
        let session = Arc::new(ShellSession {
            session_id: session_id.clone(),
            team_id,
            workdir: dir,
            created_at_ms: now,
            last_active_ms: std::sync::atomic::AtomicI64::new(now),
            exec_count: std::sync::atomic::AtomicU64::new(0),
            child: tokio::sync::Mutex::new(child),
            stdin: tokio::sync::Mutex::new(stdin),
            stdout: tokio::sync::Mutex::new(stdout),
            stderr: tokio::sync::Mutex::new(stderr),
            exec_lock: tokio::sync::Mutex::new(()),
        });
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id.clone(), Arc::clone(&session));
        emit(ShellEvent::Started(session.info()));
        Ok(session.info())
    }

    /// 停止并销毁会话。
    pub async fn stop(&self, session_id: &str, emit: &ShellEmit) -> Result<(), String> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .remove(session_id)
            .ok_or_else(|| format!("shell session not found: {session_id}"))?;
        let _ = session.stdin.lock().await.shutdown().await;
        let _ = session.child.lock().await.kill().await;
        emit(ShellEvent::Closed {
            session_id: session_id.to_string(),
            reason: "stopped".into(),
        });
        Ok(())
    }

    /// 销毁会话（进程意外退出等内部原因）。
    fn close_internal(&self, session_id: &str, reason: &str, emit: &ShellEmit) {
        if let Some(s) = self.sessions.lock().unwrap().remove(session_id) {
            // 尽力终止；不阻塞（close_internal 可能来自同步上下文）。
            let child = s.child.try_lock();
            if let Ok(mut child) = child {
                let _ = child.start_kill();
            }
            emit(ShellEvent::Closed {
                session_id: session_id.to_string(),
                reason: reason.into(),
            });
        }
    }

    /// 在会话中执行一条命令：写命令 + 哨兵，读输出直到哨兵或超时。
    /// 返回 (输出文本, 是否成功, 是否超时)。
    pub async fn exec(
        &self,
        session_id: &str,
        command: &str,
        timeout_secs: Option<u64>,
        emit: &ShellEmit,
    ) -> Result<(String, bool, bool), String> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .ok_or_else(|| format!("shell session not found: {session_id}"))?;
        let _guard = session.exec_lock.lock().await;
        // 检查进程是否还活着
        {
            let mut child = session.child.lock().await;
            if let Some(status) = child
                .try_wait()
                .map_err(|e| format!("child check failed: {e}"))?
            {
                drop(child);
                self.close_internal(session_id, &format!("exited ({status})"), emit);
                return Err(format!("shell session {session_id} has exited ({status})"));
            }
        }

        let seq = session
            .exec_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        session.last_active_ms.store(
            chrono::Utc::now().timestamp_millis(),
            std::sync::atomic::Ordering::Relaxed,
        );
        emit(ShellEvent::ExecStarted {
            session_id: session_id.to_string(),
            seq,
            command: command.to_string(),
        });

        let marker = format!("__E_SHELL_DONE_{}_{}__", session_id, uuid::Uuid::new_v4());
        let mut write_err: Option<String> = None;
        {
            let mut stdin = session.stdin.lock().await;
            let payload = format!("{command}\nprintf '\\n{marker}\\n'\n");
            if let Err(e) = stdin.write_all(payload.as_bytes()).await {
                write_err = Some(format!("write to shell failed: {e}"));
            }
            let _ = stdin.flush().await;
        }

        let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS).max(1));
        let started = std::time::Instant::now();
        let mut output = String::new();

        // 逐行读取 stdout/stderr，直到看到哨兵。读循环整体被 timeout 包裹。
        let read_task = async {
            let mut buf = String::new();
            let mut stderr_buf = String::new();
            loop {
                let stdout_line = {
                    let mut out = session.stdout.lock().await;
                    buf.clear();
                    match out.read_line(&mut buf).await {
                        Ok(0) => None,
                        Ok(_) => Some(buf.clone()),
                        Err(_) => None,
                    }
                };
                if let Some(line) = &stdout_line {
                    if line.contains(&marker) {
                        break;
                    }
                    output.push_str(line);
                    emit(ShellEvent::Output {
                        session_id: session_id.to_string(),
                        seq,
                        chunk: line.clone(),
                    });
                }
                let stderr_line = {
                    let mut err = session.stderr.lock().await;
                    stderr_buf.clear();
                    match err.read_line(&mut stderr_buf).await {
                        Ok(0) => None,
                        Ok(_) => Some(stderr_buf.clone()),
                        Err(_) => None,
                    }
                };
                if let Some(line) = &stderr_line {
                    if line.contains(&marker) {
                        break;
                    }
                    let tagged = format!("[stderr] {line}");
                    output.push_str(&tagged);
                    emit(ShellEvent::Output {
                        session_id: session_id.to_string(),
                        seq,
                        chunk: tagged,
                    });
                }
                if stdout_line.is_none() && stderr_line.is_none() {
                    // 两个流都 EOF：一次命令通常不关闭流；保底跳出避免死循环。
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    // bash 若退出，下一轮 read 返回 0 会一直 None → 只能靠上层超时。
                    // 这里检查进程是否退出，若退出则跳出。
                    let mut child = session.child.lock().await;
                    if child.try_wait().map(|s| s.is_some()).unwrap_or(false) {
                        break;
                    }
                }
            }
        };

        let outcome = tokio::time::timeout(timeout, read_task).await;
        let mut timed_out = false;
        match outcome {
            Ok(()) => {}
            Err(_) => {
                timed_out = true;
                output.push_str(&format!(
                    "\n[timeout after {}s — command may still be running]",
                    timeout.as_secs()
                ));
            }
        }

        if let Some(we) = write_err {
            return Err(we);
        }
        // 去掉尾部哨兵行（如被读入）
        if let Some(pos) = output.rfind(&marker) {
            output.truncate(pos);
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;
        emit(ShellEvent::Done {
            session_id: session_id.to_string(),
            seq,
            success: !timed_out,
            elapsed_ms,
        });
        Ok((output, !timed_out, timed_out))
    }
}

/// 进程级 ShellManager 锚点（组合根设置一次）。
static GLOBAL_SHELL: std::sync::OnceLock<Arc<ShellManager>> = std::sync::OnceLock::new();

pub fn shell_manager_global() -> Option<Arc<ShellManager>> {
    GLOBAL_SHELL.get().cloned()
}

pub fn set_shell_manager_global(mgr: Arc<ShellManager>) {
    let _ = GLOBAL_SHELL.set(mgr);
}

/// 组合根注入的 emit 回调（接到默认 agent 的 handle，广播到 Panel）。
static SHELL_EMIT: std::sync::OnceLock<ShellEmit> = std::sync::OnceLock::new();

pub fn set_shell_emit(emit: ShellEmit) {
    let _ = SHELL_EMIT.set(emit);
}

pub fn shell_emit() -> ShellEmit {
    SHELL_EMIT
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(|_| {}))
}

/// 由调用方提供翻译回调的工厂。
pub fn shell_emit_for(translate: impl Fn(ShellEvent) + Send + Sync + 'static) -> ShellEmit {
    Arc::new(translate)
}

/// 面向命令处理层：把事件通到指定 Agent 的 emit（同一 crate，无循环依赖）。
pub fn shell_emit_for_self(agent: &crate::agent::Agent) -> ShellEmit {
    let agent = unsafe {
        // 调用方持有 agent 引用，且回调在命令处理期间使用——生命周期与
        // apply_command 一致，此处通过原始指针延长（Arc 由组合根持有）。
        let ptr: *const crate::agent::Agent = agent as *const crate::agent::Agent;
        &*ptr
    };
    shell_emit_for(move |event: ShellEvent| {
        agent.emit(shell_event_to_backend(event));
    })
}

/// 把 ShellEvent 翻译为 BackendEvent。
pub fn shell_event_to_backend(event: ShellEvent) -> crate::event::BackendEvent {
    use crate::event::BackendEvent;
    match event {
        ShellEvent::Started(session) => BackendEvent::ShellSessionStarted { session },
        ShellEvent::ExecStarted {
            session_id,
            seq,
            command,
        } => BackendEvent::ShellExecStarted {
            session_id,
            seq,
            command,
        },
        ShellEvent::Output {
            session_id,
            seq,
            chunk,
        } => BackendEvent::ShellExecOutput {
            session_id,
            seq,
            chunk,
        },
        ShellEvent::Done {
            session_id,
            seq,
            success,
            elapsed_ms,
        } => BackendEvent::ShellExecDone {
            session_id,
            seq,
            success,
            elapsed_ms,
        },
        ShellEvent::Closed { session_id, reason } => {
            BackendEvent::ShellSessionClosed { session_id, reason }
        }
    }
}
