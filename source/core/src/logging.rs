//! 日志初始化：格式/级别装配、轮转文件写入器、级别热更新与 panic 兜底。
//!
//! 2026-10 架构评审：自 `main.rs` 拆出，纯搬移、行为不变。
//! 2026-10 日志治理（见 `document/core/core-logging.md`）：
//!
//! - **ANSI 自动判定**：只有输出目标是终端时才着色——此前固定开，导致
//!   journald 里的结构化字段被转义序列拆散（`grep session=` 零命中）；
//! - **panic 钩子**：panic 转一条 `ERROR` 事件（配 `log_file` 时崩溃不再
//!   只落在 stderr 里）；
//! - **级别热更新**：`SIGHUP` 重读 `[logging].level`，不用重启进程；
//! - **轮转**：按大小 **或** 按天（二者取先到），归档名带时间戳、可选
//!   gzip、超长记录切分（单条大于 `max_file_size_mb` 时跨文件续写）。

#[allow(unused_imports)]
use crate::*;
#[allow(unused_imports)]
use anyhow::Context;

/// 重载动作的类型擦除：`reload::Handle<EnvFilter, Formatter<..>>` 的第二个
/// 类型参数随输出目标（journald 的 stdout / 轮转文件）与格式（text/json）
/// 而变，存具体类型会把 `init_subscriber` 的泛型泄漏到调用方——这里统一成
/// 「给我一个新的 `EnvFilter`」这一个动作。
type LevelReloadFn = Box<dyn Fn(EnvFilter) -> Result<(), String> + Send + Sync>;

/// 级别热更新句柄：[`init_tracing`] 的返回值，组合根持有并交给
/// [`spawn_reload_on_sighup`]。
///
/// 刻意**不是**进程级静态：本仓的 P1 门禁要求零全局态（见
/// `source/core/tests/no_global_state.rs`）——能力面显式往下传，组合根
/// 拿到什么就有什么，两套宿主也互不干扰。
pub(crate) struct LogReload {
    /// 类型擦除后的重载动作（`None` = subscriber 未走可重载装配）。
    reload: Option<LevelReloadFn>,
}

impl LogReload {
    fn from_handle<S>(handle: tracing_subscriber::reload::Handle<EnvFilter, S>) -> Self
    where
        S: 'static,
    {
        Self {
            reload: Some(Box::new(move |filter: EnvFilter| {
                handle.reload(filter).map_err(|error| error.to_string())
            })),
        }
    }

    /// 热更新日志级别（`RUST_LOG` 同语法）。
    pub(crate) fn set_level(&self, spec: &str) -> Result<()> {
        let filter = EnvFilter::try_new(spec)
            .map_err(|error| anyhow::anyhow!("[logging] level 无法解析 \"{spec}\": {error}"))?;
        match self.reload.as_ref() {
            Some(reload) => {
                reload(filter).map_err(|error| anyhow::anyhow!("重载日志级别失败: {error}"))
            }
            None => anyhow::bail!("日志级别热更新未装配（subscriber 未启用 filter reloading）"),
        }
    }
}

/// 按天轮转的日期检查间隔上限（避免每条日志一次时区查询）。
const DAY_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// 轮转策略（由 `[logging]` 段解析；测试直接构造）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RotationPolicy {
    /// 单文件上限（字节）。
    pub(crate) max_bytes: u64,
    /// 保留文件总数（当前文件 + 归档）。
    pub(crate) max_files: usize,
    /// 归档是否 gzip 压缩。
    pub(crate) compress: bool,
    /// 是否按天切分（跨天首条日志自动开新文件）。
    pub(crate) by_day: bool,
}

impl RotationPolicy {
    pub(crate) fn from_config(cfg: &config::LoggingSection) -> Self {
        Self {
            max_bytes: cfg.max_file_size_mb.saturating_mul(1024 * 1024),
            max_files: cfg.max_files,
            compress: cfg.compress,
            by_day: cfg.rotate_by_day,
        }
    }
}

#[derive(Clone)]
pub(crate) struct RotatingLogWriter {
    state: Arc<std::sync::Mutex<RotatingLogState>>,
}

pub(crate) struct RotatingLogState {
    path: PathBuf,
    file: std::fs::File,
    bytes_written: u64,
    policy: RotationPolicy,
    /// 当前文件「属于」的本地日期（按天轮转判定）。
    opened_on: chrono::NaiveDate,
    /// 下次日期检查时刻（节流：见 [`DAY_CHECK_INTERVAL`]）。
    next_day_check: std::time::Instant,
}

/// 单条日志事件的写入句柄。
///
/// 缓冲而非直写：fmt 层写一条事件会调用多次 `write()`（时间戳/级别/目标/
/// 字段分开写），逐次落盘 = 每条日志多次加锁 + 多次系统调用。这里攒进
/// `buffer`，在 `flush()`/`Drop` 时一次性落盘（fmt 层不主动 `flush`，
/// 所以 `Drop` 是必须的收尾路径）。
pub(crate) struct RotatingLogGuard {
    state: Arc<std::sync::Mutex<RotatingLogState>>,
    buffer: Vec<u8>,
}

impl RotatingLogWriter {
    pub(crate) fn open(path: PathBuf, policy: RotationPolicy) -> Result<Self> {
        use anyhow::Context as _;
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create log directory {}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open log file {}", path.display()))?;
        let bytes_written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        // 已有文件的日期取 mtime：跨天重启后第一条日志就切到新文件，
        // 而不是等到「明天」才发现日期变了。
        let opened_on = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(|modified| chrono::DateTime::<chrono::Local>::from(modified).date_naive())
            .unwrap_or_else(|| chrono::Local::now().date_naive());
        Ok(Self {
            state: Arc::new(std::sync::Mutex::new(RotatingLogState {
                path,
                file,
                bytes_written,
                policy,
                opened_on,
                next_day_check: std::time::Instant::now(),
            })),
        })
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RotatingLogWriter {
    type Writer = RotatingLogGuard;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingLogGuard {
            state: Arc::clone(&self.state),
            buffer: Vec::new(),
        }
    }
}

impl RotatingLogGuard {
    /// 把缓冲落盘（每条事件一次加锁 + 一次 `write(2)`）。
    fn drain(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.buffer);
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("log writer mutex poisoned"))?;
        state.write_bytes(&data)
    }
}

impl std::io::Write for RotatingLogGuard {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.drain()
    }
}

/// fmt 层写完一条事件就丢弃 writer（不显式 `flush`）：缓冲必须在这里落盘。
impl Drop for RotatingLogGuard {
    fn drop(&mut self) {
        let _ = self.drain();
    }
}

impl RotatingLogState {
    /// 写入一段字节；必要时轮转，**绝不越过 `max_bytes`**。
    ///
    /// 单条记录大于上限时按剩余空间切开、跨文件续写（此前是「空文件直接
    /// 写超限记录」——一条巨量日志会把文件顶到上限以上）。
    fn write_bytes(&mut self, data: &[u8]) -> std::io::Result<()> {
        let mut offset = 0usize;
        while offset < data.len() {
            if self.should_rotate(data.len() - offset) {
                self.rotate()?;
            }
            // should_rotate 之后必然留有余量（max_bytes > 0 由配置校验保证）。
            let remaining = self.policy.max_bytes.saturating_sub(self.bytes_written) as usize;
            if remaining == 0 {
                self.rotate()?;
                continue;
            }
            let take = remaining.min(data.len() - offset);
            std::io::Write::write_all(&mut self.file, &data[offset..offset + take])?;
            self.bytes_written = self.bytes_written.saturating_add(take as u64);
            offset += take;
        }
        Ok(())
    }

    fn should_rotate(&mut self, incoming_bytes: usize) -> bool {
        if self.bytes_written == 0 {
            return false;
        }
        if self.bytes_written.saturating_add(incoming_bytes as u64) > self.policy.max_bytes {
            return true;
        }
        if self.policy.by_day && std::time::Instant::now() >= self.next_day_check {
            self.next_day_check = std::time::Instant::now() + DAY_CHECK_INTERVAL;
            return chrono::Local::now().date_naive() != self.opened_on;
        }
        false
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        let _ = std::io::Write::flush(&mut self.file);
        if self.path.exists() {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
            let archive = self.unique_archive(stamp);
            std::fs::rename(&self.path, &archive)?;
            if self.policy.compress {
                // 压缩失败不是致命错误：归档仍是可读的明文文件。
                if let Err(error) = compress_in_place(&archive) {
                    eprintln!(
                        "log archive compress failed ({}): {error}",
                        archive.display()
                    );
                }
            }
        }
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.bytes_written = 0;
        self.opened_on = chrono::Local::now().date_naive();
        self.next_day_check = std::time::Instant::now() + DAY_CHECK_INTERVAL;
        self.prune_archives();
        Ok(())
    }

    /// 同秒内多次轮转（超大记录连切）：追加序号避免互相覆盖。
    fn unique_archive(&self, stamp: String) -> PathBuf {
        let mut candidate = archive_path(&self.path, &stamp);
        let mut seq = 1usize;
        while candidate.exists() || candidate.with_extension("gz").exists() {
            candidate = archive_path(&self.path, &format!("{stamp}-{seq}"));
            seq += 1;
        }
        candidate
    }

    /// 保留 `max_files` 个文件（当前文件占一个位置）；多余的按时间删最旧。
    fn prune_archives(&self) {
        let keep = self.policy.max_files.saturating_sub(1);
        let archives = list_archives(&self.path);
        if archives.len() <= keep {
            return;
        }
        for stale in &archives[..archives.len() - keep] {
            let _ = std::fs::remove_file(stale);
        }
    }
}

/// 归档文件名：`<path>.<YYYYMMDD-HHMMSS>[-<seq>][.gz]`。
pub(crate) fn archive_path(path: &std::path::Path, stamp: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{stamp}"));
    PathBuf::from(name)
}

/// 列出某日志文件的全部归档（按时间升序 = 由旧到新）。
///
/// 只认 `<文件名>.<YYYYMMDD-HHMMSS>[-N][.gz]` 形态：同目录里 `app.log.tmp`
/// 之类的无关文件不会被误当成归档删掉。
fn list_archives(path: &std::path::Path) -> Vec<PathBuf> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let Some(base) = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return Vec::new();
    };
    let prefix = format!("{base}.");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut archives: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rest = name.strip_prefix(&prefix)?;
            let stamp = rest.strip_suffix(".gz").unwrap_or(rest);
            let (date, clock) = stamp.split_once('-')?;
            // 日期 8 位、时刻 6 位（同秒二次轮转带 `-<seq>` 后缀）。
            if date.len() != 8
                || !date.bytes().all(|byte| byte.is_ascii_digit())
                || clock.len() < 6
                || !clock.as_bytes()[..6].iter().all(u8::is_ascii_digit)
            {
                return None;
            }
            Some((stamp.to_string(), entry.path()))
        })
        .collect();
    archives.sort_by(|left, right| left.0.cmp(&right.0));
    archives.into_iter().map(|(_, path)| path).collect()
}
fn compress_in_place(path: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write as _;
    let source = std::fs::File::open(path)?;
    let target_path = {
        let mut name = path.as_os_str().to_os_string();
        name.push(".gz");
        PathBuf::from(name)
    };
    let target = std::fs::File::create(&target_path)?;
    let mut encoder = flate2::write::GzEncoder::new(target, flate2::Compression::default());
    std::io::copy(&mut std::io::BufReader::new(source), &mut encoder)?;
    encoder.finish()?.flush()?;
    std::fs::remove_file(path)?;
    Ok(())
}

/// panic 兜底：panic 先转一条 `ERROR` 事件，再走默认钩子（保留 stderr 追溯）。
///
/// 配了 `log_file` 时 panic 只落在 stderr 里，靠日志文件排障的人会看到
/// 「日志在崩溃前就断了」；这里让 panic 也进同一出口。
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
pub(crate) fn init_tracing(cfg: &config::LoggingSection) -> Result<LogReload> {
    install_panic_hook();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.level));
    if !cfg.log_file.is_empty() {
        let path = PathBuf::from(&cfg.log_file);
        eprintln!("logging to: {}", path.display());
        let writer = RotatingLogWriter::open(path, RotationPolicy::from_config(cfg))?;
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
) -> Result<LogReload>
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + 'static + Send + Sync,
{
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_ansi(ansi)
        .with_writer(writer);
    // 分支顺序固定：`json()` 必须在 `with_filter_reloading()` 之前——重载
    // 层的类型参数绑在「当前 formatter」上，先重载再换 formatter 不成立。
    match cfg.format.as_str() {
        "json" => {
            // JSON 出口带上当前 span：`session`/`branch_id`/`tool` 这类字段
            // 由 span 统一携带，事件本身不必重复抄写。
            let builder = builder
                .json()
                .with_current_span(true)
                .with_span_list(false)
                .with_filter_reloading();
            let handle = builder.reload_handle();
            builder.try_init().map_err(anyhow::Error::msg)?;
            Ok(LogReload::from_handle(handle))
        }
        _ => {
            let builder = builder.with_filter_reloading();
            let handle = builder.reload_handle();
            builder.try_init().map_err(anyhow::Error::msg)?;
            Ok(LogReload::from_handle(handle))
        }
    }
}

/// 注册 `SIGHUP` → 重读配置文件 `[logging].level` 并热更新。
///
/// `systemctl --user reload echo-agent-core` 或 `kill -HUP <pid>` 即可，
/// 不必重启进程（Core 重启要走最长 180s 的在途回复 drain）。
pub(crate) fn spawn_reload_on_sighup(config_path: PathBuf, reload: LogReload) {
    #[cfg(unix)]
    {
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sighup = match signal(SignalKind::hangup()) {
                Ok(signal) => signal,
                Err(error) => {
                    warn!(%error, "SIGHUP 监听装配失败：日志级别热更新不可用（需重启进程改级别）");
                    return;
                }
            };
            while sighup.recv().await.is_some() {
                match config::CoreConfig::load(&config_path) {
                    Ok(loaded) => match reload.set_level(&loaded.logging.level) {
                        Ok(()) => info!(
                            level = %loaded.logging.level,
                            "日志级别已热更新（SIGHUP）"
                        ),
                        Err(error) => warn!(%error, "日志级别热更新失败，沿用原级别"),
                    },
                    Err(error) => warn!(%error, "SIGHUP 重读配置失败，沿用原级别"),
                }
            }
        });
    }
    #[cfg(not(unix))]
    {
        // Windows：无 SIGHUP 等价物，保留函数以满足调用点。
        let _ = (config_path, reload);
        warn!("SIGHUP 日志级别热更新在非 unix 平台不可用");
    }
}

#[cfg(test)]
mod logging_tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use tracing_subscriber::fmt::MakeWriter as _;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("echo-agent-logging-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn policy(max_bytes: u64, max_files: usize, compress: bool) -> RotationPolicy {
        RotationPolicy {
            max_bytes,
            max_files,
            compress,
            by_day: false,
        }
    }

    /// 写一条「事件」：fmt 层一条日志 = 多次 write，这里用两次模拟。
    fn write_event(writer: &RotatingLogWriter, head: &[u8], tail: &[u8]) {
        let mut guard = writer.make_writer();
        guard.write_all(head).unwrap();
        guard.write_all(tail).unwrap();
        guard.flush().unwrap();
    }

    #[test]
    fn rotation_archives_with_timestamp_and_keeps_newest() {
        let dir = scratch("rotate");
        let path = dir.join("app.log");
        let writer = RotatingLogWriter::open(path.clone(), policy(8, 3, false)).unwrap();
        write_event(&writer, b"1234", b"5678");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "12345678");
        write_event(&writer, b"ne", b"xt");
        // 当前文件只剩新事件，旧事件进了带时间戳的归档。
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "next");
        let archives = list_archives(&path);
        assert_eq!(archives.len(), 1, "archives: {archives:?}");
        assert_eq!(std::fs::read_to_string(&archives[0]).unwrap(), "12345678");
        assert!(
            !archives[0].to_string_lossy().ends_with(".gz"),
            "未压缩归档不该带 .gz: {archives:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_name_parses_only_our_stamps() {
        let dir = scratch("parse");
        let path = dir.join("app.log");
        std::fs::write(&path, "").unwrap();
        std::fs::write(dir.join("app.log.20261020-120000"), "y").unwrap();
        std::fs::write(dir.join("app.log.20261019-120000.gz"), "y").unwrap();
        std::fs::write(dir.join("app.log.tmp"), "y").unwrap();
        std::fs::write(dir.join("app.log.20261021-120000-1"), "y").unwrap();
        let archives = list_archives(&path);
        assert_eq!(archives.len(), 3, "archives: {archives:?}");
        // 升序 = 由旧到新。
        assert!(archives[0].to_string_lossy().contains("20261019"));
        assert!(archives[2].to_string_lossy().contains("20261021"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_prunes_oldest_archives() {
        let dir = scratch("prune");
        let path = dir.join("app.log");
        // max_files = 2 → 当前文件 + 1 个归档。
        let writer = RotatingLogWriter::open(path.clone(), policy(4, 2, false)).unwrap();
        for round in 0..4 {
            write_event(
                &writer,
                format!("{round}{round}{round}{round}").as_bytes(),
                b"",
            );
            write_event(&writer, b"zzzz", b"");
        }
        let archives = list_archives(&path);
        assert_eq!(archives.len(), 1, "archives: {archives:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_record_is_split_across_files() {
        let dir = scratch("split");
        let path = dir.join("app.log");
        // 上限 8 字节，一条 20 字节的记录必须切开，而不是写超限。
        let writer = RotatingLogWriter::open(path.clone(), policy(8, 8, false)).unwrap();
        write_event(&writer, b"0123456789", b"abcdefghij");
        assert!(std::fs::metadata(&path).unwrap().len() <= 8);
        let mut seen = String::new();
        for archive in list_archives(&path) {
            seen.push_str(&std::fs::read_to_string(&archive).unwrap());
        }
        seen.push_str(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(seen, "0123456789abcdefghij", "记录必须完整保序跨文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compressed_archives_round_trip() {
        let dir = scratch("gzip");
        let path = dir.join("app.log");
        let writer = RotatingLogWriter::open(path.clone(), policy(4, 4, true)).unwrap();
        write_event(&writer, b"abcd", b"");
        write_event(&writer, b"efgh", b"");
        let archives = list_archives(&path);
        assert_eq!(archives.len(), 1, "archives: {archives:?}");
        assert!(archives[0].to_string_lossy().ends_with(".gz"));
        let mut decoded = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&archives[0]).unwrap())
            .read_to_string(&mut decoded)
            .unwrap();
        assert_eq!(decoded, "abcd");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
