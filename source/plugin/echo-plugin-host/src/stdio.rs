//! stdio 运输层：插件作为独立子进程运行，经 stdin/stdout 交换协议帧。
//!
//! ## 帧编码（本次定稿）
//!
//! 帧 = 4 字节**小端** `u32` 长度前缀 + JSON 帧体；前缀计的是帧体字节数，
//! JSON 形状与 [`HostToPlugin`] / [`PluginToHost`] 的 serde 表示一致。
//! 单帧上限 [`MAX_FRAME_BYTES`]（16 MiB）：读侧长度前缀超限、写侧序列化结果
//! 超限均返回 [`TransportError::Protocol`]。
//!
//! 读写经 `read_exact` / `write_all` 实现（容忍「长度先到、帧体后到」的分段到达）；
//! stdin 写入端与 stdout 读取端各由一把 [`tokio::sync::Mutex`] 串行化，
//! 并发 `send` / `recv` 不会交错损坏帧。
//!
//! ## 进程管理
//!
//! - spawn：stdin/stdout/stderr 全部管道化 + `kill_on_drop(true)`；Unix 下
//!   `process_group(0)`（插件独立成组，便于整组清理）。
//! - stderr：后台任务逐行读取，转发 `tracing::warn!`（带 `plugin = <id>` 字段）。
//! - 环境：默认**不继承**宿主环境（`env_clear`），仅透传 `PATH`；其余变量须经
//!   [`StdioTransport::with_env`] 显式传入，避免宿主环境泄漏给插件。
//! - 退出观察：对端退出后 `recv` / `send` 返回 [`TransportError::Closed`]。
//! - [`StdioConnection::shutdown`]：关闭 stdin → 等子进程退出（带 deadline）→
//!   超时 `start_kill()` + `wait()`（类比 SIGKILL 语义）并返回
//!   [`TransportError::Timeout`]；重复调用安全（幂等，第二次直接 `Ok(())`）。

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::api::{HostToPlugin, PluginToHost};
use crate::transport::{PluginConnection, PluginSpec, Transport, TransportError};

/// 单帧上限（16 MiB）。读侧长度前缀与写侧序列化结果均受此约束。
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// stdio 运输层：每次 [`Transport::start`] spawn 一个插件子进程。
///
/// 命令始终按可执行文件路径解析（[`PathBuf`]，不经 shell、无通配 / 变量展开）：
///
/// ```no_run
/// # use echo_plugin_host::stdio::StdioTransport;
/// let transport = StdioTransport::new("/usr/bin/my-plugin")
///     .with_args(["serve", "--stdio"])
///     .with_env("MY_PLUGIN_MODE", "test")
///     .with_cwd("/tmp");
/// ```
#[derive(Debug, Clone)]
pub struct StdioTransport {
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    cwd: Option<PathBuf>,
}

impl StdioTransport {
    /// 以可执行文件路径构造（相对路径按宿主当前工作目录解析）。
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
        }
    }

    /// 追加命令行参数（链式；多次调用累积）。
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// 追加一个子进程环境变量（链式；宿主环境默认不继承，仅透传 `PATH`）。
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// 设置子进程工作目录（链式）。
    pub fn with_cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// 组装 spawn 命令（管道 stdio / 最小环境 / 进程组 / 工作目录）。
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // 最小环境：不继承宿主环境，仅透传 PATH（可执行查找 / 插件自身依赖）；
        // 其余变量经 `with_env` 显式声明，不额外泄漏。
        cmd.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        #[cfg(unix)]
        cmd.process_group(0); // 独立进程组：shutdown 超时强杀 / 整组清理的前提。
        cmd
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn start(&self, spec: PluginSpec) -> Result<Box<dyn PluginConnection>, TransportError> {
        let mut cmd = self.command();
        let mut child = cmd.spawn().map_err(|e| {
            TransportError::Io(format!(
                "failed to spawn plugin process {:?}: {e}",
                self.program
            ))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Io("plugin stdin pipe unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Io("plugin stdout pipe unavailable".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| TransportError::Io("plugin stderr pipe unavailable".to_string()))?;

        spawn_stderr_forwarder(&spec.plugin_id, stderr);

        Ok(Box::new(StdioConnection {
            stdin: Mutex::new(Some(stdin)),
            stdout: Mutex::new(stdout),
            child: Mutex::new(Some(child)),
        }))
    }
}

/// 宿主侧 stdio 连接：stdin 写入端与 stdout 读取端各一把锁，避免并发交错损坏帧。
pub struct StdioConnection {
    /// 宿主 → 插件（stdin）；`None` = shutdown 已关闭该方向。
    stdin: Mutex<Option<ChildStdin>>,
    /// 插件 → 宿主（stdout）。
    stdout: Mutex<ChildStdout>,
    /// 子进程句柄；`None` = 已收割（shutdown 完成或已强杀）。
    child: Mutex<Option<Child>>,
}

impl StdioConnection {
    /// 读一帧：长度前缀（4 字节 LE）→ 帧体 → JSON 解码。
    ///
    /// 分段到达由 `read_exact` 等待补齐；EOF（对端退出）= [`TransportError::Closed`]。
    async fn read_frame(&self) -> Result<PluginToHost, TransportError> {
        let mut stdout = self.stdout.lock().await;

        let mut len_bytes = [0u8; 4];
        stdout
            .read_exact(&mut len_bytes)
            .await
            .map_err(|e| map_read_error("read frame length", e))?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(TransportError::Protocol(format!(
                "frame length {len} exceeds limit {MAX_FRAME_BYTES}"
            )));
        }

        let mut body = vec![0u8; len];
        stdout
            .read_exact(&mut body)
            .await
            .map_err(|e| map_read_error("read frame body", e))?;

        serde_json::from_slice(&body)
            .map_err(|e| TransportError::Protocol(format!("invalid JSON frame: {e}")))
    }

    /// 写一帧：JSON 编码 → 4 字节 LE 长度前缀 + 帧体（同一把锁内一次性写出）。
    async fn write_frame(&self, msg: &HostToPlugin) -> Result<(), TransportError> {
        let body = serde_json::to_vec(msg)
            .map_err(|e| TransportError::Protocol(format!("encode message: {e}")))?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(TransportError::Protocol(format!(
                "frame length {} exceeds limit {MAX_FRAME_BYTES}",
                body.len()
            )));
        }

        let mut stdin = self.stdin.lock().await;
        let Some(pipe) = stdin.as_mut() else {
            return Err(TransportError::Closed);
        };
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);
        pipe.write_all(&frame).await.map_err(map_write_error)?;
        pipe.flush().await.map_err(map_write_error)
    }
}

#[async_trait]
impl PluginConnection for StdioConnection {
    async fn send(&self, msg: HostToPlugin) -> Result<(), TransportError> {
        self.write_frame(&msg).await
    }

    async fn recv(&self) -> Result<PluginToHost, TransportError> {
        self.read_frame().await
    }

    /// 关停语义（类比 SIGTERM → SIGKILL）：
    /// 1. 丢弃 stdin（插件收到 EOF，可自行退出）；
    /// 2. 等待子进程退出，最多 `deadline`；
    /// 3. 超时 → `start_kill()` 强杀 + `wait()` 收尸，返回 [`TransportError::Timeout`]。
    ///
    /// 重复调用：子进程已收割时直接 `Ok(())`（幂等）。
    async fn shutdown(&self, deadline: Duration) -> Result<(), TransportError> {
        // 关闭宿主 → 插件方向（stdin EOF）。
        self.stdin.lock().await.take();

        let mut guard = self.child.lock().await;
        let Some(mut child) = guard.take() else {
            return Ok(()); // 已关停 / 已退出：幂等
        };
        match tokio::time::timeout(deadline, child.wait()).await {
            Ok(Ok(_status)) => Ok(()),
            Ok(Err(e)) => Err(TransportError::Io(format!("wait for plugin exit: {e}"))),
            Err(_elapsed) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Err(TransportError::Timeout)
            }
        }
    }
}

/// stderr 转发：后台任务逐行读取，带插件 id 前缀转 `tracing::warn!`。
fn spawn_stderr_forwarder(plugin_id: &str, stderr: tokio::process::ChildStderr) {
    let plugin_id = plugin_id.to_string();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => tracing::warn!(plugin = %plugin_id, "[stderr] {}", line),
                Ok(None) => break, // EOF：子进程退出（或关闭了 stderr）
                Err(e) => {
                    tracing::debug!(plugin = %plugin_id, error = %e, "stderr read ended");
                    break;
                }
            }
        }
    });
}

/// 读错误映射：EOF / 管道破裂 = 对端退出（`Closed`）；其余 = IO 错误。
fn map_read_error(what: &str, e: std::io::Error) -> TransportError {
    match e.kind() {
        ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {
            TransportError::Closed
        }
        _ => TransportError::Io(format!("{what}: {e}")),
    }
}

/// 写错误映射：管道破裂 = 对端退出（`Closed`）；其余 = IO 错误。
fn map_write_error(e: std::io::Error) -> TransportError {
    match e.kind() {
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof => {
            TransportError::Closed
        }
        _ => TransportError::Io(format!("write frame: {e}")),
    }
}
