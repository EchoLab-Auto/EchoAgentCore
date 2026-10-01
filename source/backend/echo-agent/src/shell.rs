//! 后台 shell 会话 —— 持久的 bash 进程，供面板/LLM 反复执行命令。
//!
//! 设计要点：
//! - 每个会话 = 一个 `bash --noprofile --norc` 子进程（stdin/stdout/stderr 管道），
//!   spawn 时为独立进程组（`process_group(0)`），销毁时整组终止（killpg）
//! - 命令执行：写命令 + 随机哨兵标记（stdout/stderr 双写；两路哨兵收齐
//!   才算命令结束，保证尾部 stderr 不因两管道竞争被漏收）；
//!   stdout/stderr 由两路专职读取任务并发消费（2026-10-01 重写——旧实现
//!   交替顺序读两流，任一流空闲都会永久阻塞读循环，致每条命令假超时）
//! - 会话串行执行（一次一条命令）；不同会话并行；上限 [`MAX_SHELL_SESSIONS`]
//! - 事件通过注入的 emit 回调广播（组合根接到默认 agent 的 handle），
//!   Panel 据此做终端可视化（流式输出）
//! - 用户可 `ShellStop` 销毁（终止 bash 及全部子孙）；进程意外退出时
//!   自动清理残余进程组并广播关闭事件

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use crate::event::ShellSessionInfo;

pub const MAX_SHELL_SESSIONS: usize = 8;
/// 单条命令默认超时（秒）。
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 120;
/// 每会话输出行缓冲上限：两次 exec 之间后台进程输出堆积的背压水位
/// （缓冲满后读取任务暂停消费、经管道反压子进程，防止无界内存增长）。
const OUTPUT_BUFFER_LINES: usize = 4096;
/// 哨兵双写 stdout/stderr；收到其一后，为等待另一路允许的静默时长
/// （毫秒）。另一路迟迟不来（如 stderr 被重定向出会话管道）时按此兜底。
const SENTINEL_QUIET_MS: u64 = 500;
/// 等待另一路哨兵的宽限上限（毫秒）；期间持续有新输出会延长，封顶此值。
const SENTINEL_GRACE_CAP_MS: u64 = 3000;

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

/// 输出行来源流。
#[derive(Clone, Copy, Debug)]
enum StreamKind {
    Stdout,
    Stderr,
}

/// 读取任务汇入通道的一条输出行（行尾 `\n` 已由读取侧剥除）。
struct OutputLine {
    stream: StreamKind,
    text: String,
}

/// 为单个输出流启动专职读取任务：逐行读取并汇入通道。
/// stdout/stderr 各一个任务并发消费是「单流空闲不阻塞另一流」的关键；
/// 通道满时任务在此 await（对子进程形成反压）；接收端销毁（会话关闭）
/// 或流 EOF / 读错误时任务退出。
fn spawn_output_reader<R>(reader: R, stream: StreamKind, tx: mpsc::Sender<OutputLine>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(text)) = lines.next_line().await {
            if tx.send(OutputLine { stream, text }).await.is_err() {
                break;
            }
        }
    });
}

/// 向会话进程组发送 SIGKILL（pgid 由 spawn 时 `process_group(0)` 确定）。
/// 组内是 bash 及其全部子孙；自行 `setsid` 脱离进程组的后代不在其列。
#[cfg(unix)]
fn kill_process_group(pgid: Option<i32>) {
    if let Some(pgid) = pgid {
        // SAFETY: 负 pid 表示向进程组发信号；该组由我们通过
        // process_group(0) 创建，组内都是本会话托管的进程。
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pgid: Option<i32>) {}

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
    /// 会话进程组 id（= bash pid，经 `process_group(0)` 建组）；
    /// 销毁时 `killpg` 终止 bash 及其全部子孙。
    pgid: Option<i32>,
    stdin: tokio::sync::Mutex<ChildStdin>,
    /// stdout/stderr 两路读取任务汇入的输出通道；exec 从通道取行至哨兵。
    rx: tokio::sync::Mutex<mpsc::Receiver<OutputLine>>,
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
        let mut command = Command::new("bash");
        command
            .arg("--noprofile")
            .arg("--norc")
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // 独立进程组：会话内进程与 bash 同组，ShellStop / 意外退出时
        // killpg 一次清整棵进程树（此前只杀 bash 本身，后台进程变孤儿续存）。
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("spawn bash failed: {e}"))?;
        let stdin = child.stdin.take().ok_or("bash stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("bash stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("bash stderr unavailable")?;
        // pid == pgid（进程组由上面的 process_group(0) 建立）。
        let pgid = child.id().map(|pid| pid as i32);
        let seq = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let session_id = format!("sh-{seq}");
        let now = chrono::Utc::now().timestamp_millis();
        // 两路读取任务随会话建立、与生共死；通道容量即背压水位。
        let (tx, rx) = mpsc::channel::<OutputLine>(OUTPUT_BUFFER_LINES);
        spawn_output_reader(stdout, StreamKind::Stdout, tx.clone());
        spawn_output_reader(stderr, StreamKind::Stderr, tx);
        let session = Arc::new(ShellSession {
            session_id: session_id.clone(),
            team_id,
            workdir: dir,
            created_at_ms: now,
            last_active_ms: std::sync::atomic::AtomicI64::new(now),
            exec_count: std::sync::atomic::AtomicU64::new(0),
            child: tokio::sync::Mutex::new(child),
            pgid,
            stdin: tokio::sync::Mutex::new(stdin),
            rx: tokio::sync::Mutex::new(rx),
            exec_lock: tokio::sync::Mutex::new(()),
        });
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id.clone(), Arc::clone(&session));
        emit(ShellEvent::Started(session.info()));
        Ok(session.info())
    }

    /// 停止并销毁会话（整组终止：bash 及其全部子孙，含后台任务）。
    pub async fn stop(&self, session_id: &str, emit: &ShellEmit) -> Result<(), String> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .remove(session_id)
            .ok_or_else(|| format!("shell session not found: {session_id}"))?;
        kill_process_group(session.pgid);
        let _ = session.stdin.lock().await.shutdown().await;
        {
            let mut child = session.child.lock().await;
            // 组已杀时为兜底清理（如 pgid 不可用）；wait 回收直接子进程防僵尸。
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        emit(ShellEvent::Closed {
            session_id: session_id.to_string(),
            reason: "stopped".into(),
        });
        Ok(())
    }

    /// 销毁会话（进程意外退出等内部原因）。
    fn close_internal(&self, session_id: &str, reason: &str, emit: &ShellEmit) {
        if let Some(s) = self.sessions.lock().unwrap().remove(session_id) {
            // 整组清理：bash 可能已退出，残余后台子孙同样要收掉。
            // 不阻塞（close_internal 可能来自同步上下文）。
            kill_process_group(s.pgid);
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

    /// 在会话中执行一条命令：写命令 + 哨兵，从输出通道读行直到哨兵或超时。
    /// 超时只中止读取、保留会话（命令可能仍在运行）。
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
            // 哨兵双写：stdout 一份、stderr 一份（两路收齐才判命令结束）。
            // 不带前导换行——正常输出以换行结尾时哨兵独占一行、零噪音；
            // 命令末尾未换行时哨兵会拼在残余文本之后，消费侧按前缀切分。
            let payload = format!("{command}\nprintf '{marker}\\n'\nprintf '{marker}\\n' 1>&2\n");
            if let Err(e) = stdin.write_all(payload.as_bytes()).await {
                write_err = Some(format!("write to shell failed: {e}"));
            }
            let _ = stdin.flush().await;
        }

        let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS).max(1));
        let started = std::time::Instant::now();
        let mut output = String::new();

        // 从通道消费输出行直到双哨兵收齐；整体受 timeout 约束。
        // 迟到哨兵（前一条超时命令遗留）按前缀识别并跳过，不外泄。
        // 宽限：只收到一路哨兵时，为另一路留一段静默窗口（另一路可能因
        // 读取任务调度稍慢而滞后；stderr 被移出会话管道则窗口静默到期收尾），
        // 期间仍有新行到达则顺延，总宽限封顶 SENTINEL_GRACE_CAP_MS。
        let sentinel_prefix = format!("__E_SHELL_DONE_{session_id}_");
        let deadline = tokio::time::Instant::now() + timeout;
        let mut timed_out = false;
        let mut seen_out = false;
        let mut seen_err = false;
        let mut grace_start: Option<tokio::time::Instant> = None;
        let mut grace_deadline: Option<tokio::time::Instant> = None;
        {
            let mut rx = session.rx.lock().await;
            loop {
                let wait_until = match grace_deadline {
                    Some(g) => deadline.min(g),
                    None => deadline,
                };
                match tokio::time::timeout_at(wait_until, rx.recv()).await {
                    Err(_) => {
                        let now = tokio::time::Instant::now();
                        if let Some(g) = grace_deadline {
                            if now < g {
                                // 只可能是整体超时先到（wait_until 取了两者最小）。
                                timed_out = true;
                                output.push_str(&format!(
                                    "\n[timeout after {}s — command may still be running]",
                                    timeout.as_secs()
                                ));
                            }
                        } else {
                            timed_out = true;
                            output.push_str(&format!(
                                "\n[timeout after {}s — command may still be running]",
                                timeout.as_secs()
                            ));
                        }
                        break;
                    }
                    // 通道关闭：两路读取任务都已结束（进程侧管道全关）。
                    Ok(None) => break,
                    Ok(Some(line)) => {
                        // 本命令 / 过期哨兵行：其前的残余文本（命令末尾未换行时
                        // 会与哨兵拼在同一行）照常收集，哨兵文本本身不进输出。
                        let mut own_sentinel = false;
                        let mut text: Option<&str> = None;
                        if let Some(pos) = line.text.find(marker.as_str()) {
                            own_sentinel = true;
                            if pos > 0 {
                                text = Some(&line.text[..pos]);
                            }
                        } else if let Some(pos) = line.text.find(sentinel_prefix.as_str()) {
                            if pos > 0 {
                                text = Some(&line.text[..pos]);
                            }
                        } else {
                            text = Some(&line.text);
                        }
                        if let Some(text) = text {
                            let chunk = match line.stream {
                                StreamKind::Stdout => format!("{text}\n"),
                                StreamKind::Stderr => format!("[stderr] {text}\n"),
                            };
                            output.push_str(&chunk);
                            emit(ShellEvent::Output {
                                session_id: session_id.to_string(),
                                seq,
                                chunk,
                            });
                        }
                        if own_sentinel {
                            match line.stream {
                                StreamKind::Stdout => seen_out = true,
                                StreamKind::Stderr => seen_err = true,
                            }
                        }
                        if seen_out && seen_err {
                            break;
                        }
                        if seen_out || seen_err {
                            let now = tokio::time::Instant::now();
                            let start = *grace_start.get_or_insert(now);
                            grace_deadline = Some(
                                (now + Duration::from_millis(SENTINEL_QUIET_MS))
                                    .min(start + Duration::from_millis(SENTINEL_GRACE_CAP_MS)),
                            );
                        }
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn quiet_emit() -> ShellEmit {
        Arc::new(|_| {})
    }

    async fn start_session(mgr: &ShellManager) -> String {
        mgr.start(Some("/tmp".into()), None, &quiet_emit())
            .await
            .expect("start session")
            .session_id
    }

    /// 进程存在性检查（信号 0，不实际发送信号）。
    #[cfg(unix)]
    fn process_alive(pid: i32) -> bool {
        // SAFETY: 信号 0 只做存在性检查。
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// 回归：stderr 空闲时不得阻塞读循环（旧实现交替顺序读两流，
    /// 任一流空闲即死锁——表现为每条命令都假超时）。
    #[tokio::test]
    async fn exec_returns_promptly_when_stderr_silent() {
        let mgr = ShellManager::new();
        let sid = start_session(&mgr).await;
        let t0 = Instant::now();
        let (out, success, timed_out) = mgr
            .exec(&sid, "echo hello", Some(20), &quiet_emit())
            .await
            .expect("exec");
        assert!(!timed_out, "stderr 静默不得导致超时（输出: {out:?}）");
        assert!(success);
        assert!(out.contains("hello"));
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "echo 应立即返回，实际 {:?}",
            t0.elapsed()
        );
        mgr.stop(&sid, &quiet_emit()).await.expect("stop");
    }

    /// stdout/stderr 交替输出均被捕获：stderr 行带 [stderr] 前缀，哨兵不外泄。
    #[tokio::test]
    async fn exec_captures_both_streams_tagged() {
        let mgr = ShellManager::new();
        let sid = start_session(&mgr).await;
        let (out, _, timed_out) = mgr
            .exec(
                &sid,
                "echo O1; echo E1 1>&2; echo O2; echo E2 1>&2",
                Some(20),
                &quiet_emit(),
            )
            .await
            .expect("exec");
        assert!(!timed_out);
        assert!(out.contains("O1") && out.contains("O2"));
        assert!(out.contains("[stderr] E1") && out.contains("[stderr] E2"));
        assert!(!out.contains("__E_SHELL_DONE_"), "哨兵不得外泄: {out:?}");
        mgr.stop(&sid, &quiet_emit()).await.expect("stop");
    }

    /// 会话内状态保持（cd / 环境变量跨命令生效）。
    #[tokio::test]
    async fn session_keeps_state_across_execs() {
        let mgr = ShellManager::new();
        let sid = start_session(&mgr).await;
        mgr.exec(&sid, "cd /tmp && export ECHO_T=42", Some(20), &quiet_emit())
            .await
            .expect("exec1");
        let (out, _, timed_out) = mgr
            .exec(&sid, "pwd; echo $ECHO_T", Some(20), &quiet_emit())
            .await
            .expect("exec2");
        assert!(!timed_out);
        assert!(out.contains("/tmp"));
        assert!(out.contains("42"));
        mgr.stop(&sid, &quiet_emit()).await.expect("stop");
    }

    /// 超时只中止读取、保留会话；迟到的上一命令哨兵行不外泄。
    #[tokio::test]
    async fn timeout_keeps_session_and_stale_sentinel_is_hidden() {
        let mgr = ShellManager::new();
        let sid = start_session(&mgr).await;
        let (_, _, timed_out) = mgr
            .exec(&sid, "sleep 2", Some(1), &quiet_emit())
            .await
            .expect("exec1");
        assert!(timed_out, "sleep 2 在 1s 超时下应超时");
        // 会话仍可用：sleep 2 结束后命令照常执行、输出照常捕获。
        let (out, _, timed_out) = mgr
            .exec(&sid, "echo alive", Some(20), &quiet_emit())
            .await
            .expect("exec2");
        assert!(!timed_out);
        assert!(out.contains("alive"));
        assert!(
            !out.contains("__E_SHELL_DONE_"),
            "迟到哨兵不得外泄: {out:?}"
        );
        mgr.stop(&sid, &quiet_emit()).await.expect("stop");
    }

    /// stderr 大突发（500 行）不丢尾：双哨兵收齐语义保证尾部 stderr
    /// 不因两管道读取调度竞争被漏收（曾出现的偶发截断回归）。
    #[tokio::test]
    async fn large_stderr_burst_is_fully_captured() {
        let mgr = ShellManager::new();
        let sid = start_session(&mgr).await;
        let (out, _, timed_out) = mgr
            .exec(
                &sid,
                "for i in $(seq 1 500); do echo \"E$i\" 1>&2; done; echo OUT-LAST",
                Some(20),
                &quiet_emit(),
            )
            .await
            .expect("exec");
        assert!(!timed_out);
        assert!(out.contains("OUT-LAST"));
        assert!(out.contains("[stderr] E1\n"), "首行 E1 不应丢失");
        assert!(out.contains("[stderr] E500"), "尾行 E500 不应丢失");
        mgr.stop(&sid, &quiet_emit()).await.expect("stop");
    }

    /// ShellStop 终止整个进程组：bash 的后台任务一并死亡。
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_kills_entire_process_group() {
        let mgr = ShellManager::new();
        let sid = start_session(&mgr).await;
        let (out, _, _) = mgr
            .exec(&sid, "sleep 300 & echo $!", Some(10), &quiet_emit())
            .await
            .expect("exec");
        let pid: i32 = out
            .trim()
            .lines()
            .last()
            .expect("pid line")
            .trim()
            .parse()
            .expect("parse pid");
        assert!(process_alive(pid), "sleep 应先存活");
        mgr.stop(&sid, &quiet_emit()).await.expect("stop");
        let mut dead = false;
        for _ in 0..60 {
            if !process_alive(pid) {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            dead,
            "ShellStop 后进程组内后台任务应被终止（pid {pid} 仍存活）"
        );
    }

    /// 会话上限：第 9 个会话被拒绝。
    #[tokio::test]
    async fn session_limit_enforced() {
        let mgr = ShellManager::new();
        let mut sids = Vec::new();
        for _ in 0..MAX_SHELL_SESSIONS {
            sids.push(start_session(&mgr).await);
        }
        let err = mgr
            .start(None, None, &quiet_emit())
            .await
            .expect_err("应超限");
        assert!(err.contains("limit reached"), "err = {err}");
        for sid in sids {
            mgr.stop(&sid, &quiet_emit()).await.expect("stop");
        }
    }

    /// 流式事件顺序：ExecStarted → Output（先于 Done）→ Done → Closed。
    #[tokio::test]
    async fn streams_output_events_before_done() {
        #[derive(PartialEq, Debug)]
        enum Ev {
            SessionCreated,
            ExecStarted,
            Output,
            Done,
            Closed,
        }
        let log: Arc<std::sync::Mutex<Vec<Ev>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log2 = Arc::clone(&log);
        let emit: ShellEmit = Arc::new(move |e| {
            let tag = match e {
                ShellEvent::Started(_) => Ev::SessionCreated,
                ShellEvent::ExecStarted { .. } => Ev::ExecStarted,
                ShellEvent::Output { .. } => Ev::Output,
                ShellEvent::Done { .. } => Ev::Done,
                ShellEvent::Closed { .. } => Ev::Closed,
            };
            log2.lock().unwrap().push(tag);
        });
        let mgr = ShellManager::new();
        let sid = mgr
            .start(Some("/tmp".into()), None, &emit)
            .await
            .expect("start")
            .session_id;
        mgr.exec(&sid, "echo streamed", Some(20), &emit)
            .await
            .expect("exec");
        mgr.stop(&sid, &emit).await.expect("stop");
        let log = log.lock().unwrap();
        let first_output = log
            .iter()
            .position(|e| *e == Ev::Output)
            .expect("有 Output");
        let done = log.iter().position(|e| *e == Ev::Done).expect("有 Done");
        assert!(first_output < done, "Output 先于 Done: {log:?}");
        assert_eq!(log.last(), Some(&Ev::Closed));
    }
}
