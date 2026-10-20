//! 日志初始化：按大小轮转的文件写入器 + tracing subscriber 装配。
//!
//! 2026-10 架构评审：自 `main.rs` 拆出，纯搬移、行为不变。
//! 2026-10 日志治理（见 `document/core/core-logging.md`）：
//!
//! - **ANSI 自动判定**：只有输出目标是终端时才着色——此前固定开，导致
//!   journald 里的结构化字段被转义序列拆散（`grep session=` 零命中）；
//! - **panic 钩子**：panic 转一条 `ERROR` 事件（配 `log_file` 时崩溃不再
//!   只落在 stderr 里）。

#[allow(unused_imports)]
use crate::*;
#[allow(unused_imports)]
use anyhow::Context;

#[derive(Clone)]
pub(crate) struct RotatingLogWriter {
    state: Arc<std::sync::Mutex<RotatingLogState>>,
}

pub(crate) struct RotatingLogState {
    path: PathBuf,
    file: std::fs::File,
    bytes_written: u64,
    max_bytes: u64,
    max_files: usize,
}

pub(crate) struct RotatingLogGuard {
    state: Arc<std::sync::Mutex<RotatingLogState>>,
}

impl RotatingLogWriter {
    pub(crate) fn open(path: PathBuf, max_bytes: u64, max_files: usize) -> Result<Self> {
        use anyhow::Context as _;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create log directory {}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open log file {}", path.display()))?;
        let bytes_written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        Ok(Self {
            state: Arc::new(std::sync::Mutex::new(RotatingLogState {
                path,
                file,
                bytes_written,
                max_bytes,
                max_files,
            })),
        })
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RotatingLogWriter {
    type Writer = RotatingLogGuard;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingLogGuard {
            state: Arc::clone(&self.state),
        }
    }
}

impl std::io::Write for RotatingLogGuard {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("log writer mutex poisoned"))?;
        state.rotate_if_needed(buffer.len() as u64)?;
        let written = std::io::Write::write(&mut state.file, buffer)?;
        state.bytes_written = state.bytes_written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("log writer mutex poisoned"))?;
        std::io::Write::flush(&mut state.file)
    }
}

impl RotatingLogState {
    pub(crate) fn rotate_if_needed(&mut self, incoming_bytes: u64) -> std::io::Result<()> {
        if self.bytes_written == 0
            || self.bytes_written.saturating_add(incoming_bytes) <= self.max_bytes
        {
            return Ok(());
        }
        std::io::Write::flush(&mut self.file)?;
        let oldest = rotated_log_path(&self.path, self.max_files);
        if oldest.exists() {
            std::fs::remove_file(oldest)?;
        }
        for index in (1..self.max_files).rev() {
            let source = rotated_log_path(&self.path, index);
            let destination = rotated_log_path(&self.path, index + 1);
            if source.exists() {
                let _ = std::fs::rename(source, destination);
            }
        }
        if self.path.exists() {
            std::fs::rename(&self.path, rotated_log_path(&self.path, 1))?;
        }
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.bytes_written = 0;
        Ok(())
    }
}

pub(crate) fn rotated_log_path(path: &std::path::Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

/// panic 兜底：panic 先转一条 `ERROR` 事件，再走默认钩子（保留 stderr 追溯）。
///
/// 配了 `log_file` 时 panic 只落在 stderr 里，靠日志文件排障的人会看到
/// 「日志在崩溃前就断了」；这里让 panic 也进同一出口。
///
/// 只应由 [`init_tracing`] 调用（每进程一次，重复安装会让同一条 panic 被多
/// 打一遍）。这里不设静态哨兵：本仓 P1 门禁禁止进程级静态态，而
/// `init_tracing` 在 `main` 里本就只调用一次。
pub(crate) fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "panic caught by logging hook");
        default_hook(info);
    }));
}

/// Core defaults to stdout/journald, but honors an explicit `log_file` with
/// rotating files.
pub(crate) fn init_tracing(cfg: &config::LoggingSection) -> Result<()> {
    install_panic_hook();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.level));
    if !cfg.log_file.is_empty() {
        let path = PathBuf::from(&cfg.log_file);
        eprintln!("logging to: {}", path.display());
        let writer = RotatingLogWriter::open(
            path,
            cfg.max_file_size_mb.saturating_mul(1024 * 1024),
            cfg.max_files,
        )?;
        init_subscriber(cfg, filter, writer, false)
    } else {
        // journald / 管道 / 重定向：ANSI 只在真正的终端上才有意义。
        // 此前固定 `true`，journald 里 97% 的行都夹着转义序列，
        // 结构化字段（`session=` / `elapsed_ms=`）因此不可 grep。
        init_subscriber(
            cfg,
            filter,
            std::io::stdout,
            std::io::IsTerminal::is_terminal(&std::io::stdout()),
        )
    }
}

pub(crate) fn init_subscriber<W>(
    cfg: &config::LoggingSection,
    filter: EnvFilter,
    writer: W,
    ansi: bool,
) -> Result<()>
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + 'static + Send + Sync,
{
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_ansi(ansi)
        .with_writer(writer);
    match cfg.format.as_str() {
        "json" => builder.json().try_init().map_err(anyhow::Error::msg)?,
        _ => builder.try_init().map_err(anyhow::Error::msg)?,
    }
    Ok(())
}

#[cfg(test)]
mod logging_tests {
    use super::*;
    use std::io::Write as _;
    use tracing_subscriber::fmt::MakeWriter as _;

    #[test]
    pub(crate) fn rotating_writer_keeps_numbered_history() {
        let path = std::env::temp_dir().join(format!(
            "echo-agent-rotation-test-{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(rotated_log_path(&path, 1));
        let writer = RotatingLogWriter::open(path.clone(), 8, 2).unwrap();
        {
            let mut guard = writer.make_writer();
            guard.write_all(b"12345678").unwrap();
            guard.write_all(b"next").unwrap();
            guard.flush().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "next");
        assert_eq!(
            std::fs::read_to_string(rotated_log_path(&path, 1)).unwrap(),
            "12345678"
        );
        drop(writer);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(rotated_log_path(&path, 1));
    }
}
