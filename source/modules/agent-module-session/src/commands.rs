//! 会话写命令域：`ClearHistory` / `ArchiveHistory` / `CompactHistory`。
//!
//! ## 为什么这三条归会话模块（2026-11 S6 补口）
//!
//! 它们此前留在宿主 `agent/commands.rs`（约 230 行），理由是「动的是会话
//! **真源**与 turn 驱动（宿主骨架）」。会话子系统外置成 `agent-module-session`
//! 之后这个理由不成立了：真源就是本模块的 `TrunkStore`，
//! `clear_session` / `archive_session` 都是它的方法。
//!
//! 现在宿主只保留**三件只有它做得了的事**（[`SessionCommandHost`]）：
//!
//! | 宿主提供 | 为什么只有它能做 |
//! |---|---|
//! | `resolve_trunk(team)` | 人格注册表在宿主（`AgentManager`），模块不认识它 |
//! | `emit_context_snapshot` | 快照要算 token 预算、走上下文块装配 |
//! | `compact` | 压缩策略在 `agent-module-loop`、provider 在宿主——模块不能依赖模块 |
//! | `forget_session_provider` | 会话级 API 覆盖表在 `Agent.model` |
//!
//! 其余（校验 session_id、fail-closed 文案、清理后重发时间线、进度事件、
//! 回执文案）全在这里。
//!
//! ## 一条不变量
//!
//! **`session_id` 缺失 = 拒绝执行**（不是降级成"整个 persona"）。面板的按钮
//! 承诺的是"清理/归档/压缩**当前会话**"，旧客户端不带 session 时若退化，会
//! 一次抹掉该人格下所有会话（2026-10 用户报障）。

use std::sync::Arc;

use agent_module_management::protocol::timeline_text::elide_reasoning_for_wire;
use agent_module_management::protocol::{BackendCommand, BackendEvent};
use echo_session::CompactionPhase;

use crate::store::TrunkStore;

/// 本模块处理的命令名。
pub const COMMANDS: [&str; 3] = ["ClearHistory", "ArchiveHistory", "CompactHistory"];

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// 宿主接缝：只有宿主做得了的那几件事。
pub trait SessionCommandHost: Send + Sync {
    /// 命令到达的人格 id（`None` = 未归因的嵌入式宿主）。
    fn self_team_id(&self) -> Option<String>;

    /// 解析目标人格的 trunk。
    ///
    /// `None` = 该 team 不存在（调用方报错，不回退到自身——那会把 A 人格的
    /// 历史按 B 人格的意图清掉）。
    fn resolve_trunk(&self, team_id: &str) -> Option<Arc<TrunkStore>>;

    /// 本宿主的 trunk（team 未指定时的目标）。
    fn trunk(&self) -> Arc<TrunkStore>;

    /// 忘掉会话级 API 覆盖（历史没了，覆盖不能留着——否则该会话静默复用
    /// 旧 profile）。
    fn forget_session_provider(&self, session_id: &str);

    /// 暂停目标会话的 turn 写者；返回的守卫必须持有到操作和回执结束。
    /// 宿主必须取消旧 turn、等待其释放串行闸门；超时不得执行清理。
    fn quiesce_session<'a>(
        &'a self,
        team_id: Option<&'a str>,
        session_id: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn Send>, String>>;

    /// 发一条线协议事件。
    fn emit(&self, event: BackendEvent);

    /// 发该会话的上下文快照（要算 token，宿主独占）。
    fn emit_context_snapshot<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()>;

    /// 压缩单个会话（要 LLM provider + 压缩策略模块）。
    fn compact<'a>(
        &'a self,
        team_id: Option<String>,
        session_id: &'a str,
        keep_recent: usize,
    ) -> BoxFuture<'a, Result<String, String>>;
}

/// 一条命令要作用的对象：`(team_id, trunk)`。
struct Target {
    team_id: Option<String>,
    trunk: Arc<TrunkStore>,
}

impl Target {
    /// 解析目标并**确保它的会话数据已加载**（2026-11 懒加载）。
    ///
    /// 会话写命令（清理 / 归档 / 压缩）全都要读改事件日志：未加载时动手改的
    /// 只是一份空内存，而落盘又被"文件已存在且没读过"的守卫挡住——命令会
    /// 报成功却什么都没发生。所以三个命令统一走这里。
    ///
    /// 跨组闭环（2026-11）：用 `ensure_loaded_or_err`——加载失败（坏档 /
    /// 读失败）必须把原因**交还调用方**（发 `BackendEvent::Error`），
    /// 不能"命令报成功、实际什么都没写"。失败态是负缓存，不会每轮重试，
    /// 管理修复出口（`retry_failed_load` / `recover_failed_load`）不受影响。
    async fn resolve_loaded(
        host: &Arc<dyn SessionCommandHost>,
        team_id: Option<String>,
    ) -> Result<Self, String> {
        let target = Self::resolve(host, team_id)?;
        target.trunk.ensure_loaded_or_err().await?;
        Ok(target)
    }

    /// 按 `team_id` 解析目标；解析失败返回用户可读的失败原因。
    fn resolve(
        host: &Arc<dyn SessionCommandHost>,
        team_id: Option<String>,
    ) -> Result<Self, String> {
        match team_id {
            None => Ok(Self {
                team_id: None,
                trunk: host.trunk(),
            }),
            Some(id) => match host.resolve_trunk(&id) {
                Some(trunk) => Ok(Self {
                    team_id: Some(id),
                    trunk,
                }),
                None if host.self_team_id().as_deref() == Some(id.as_str()) => Ok(Self {
                    team_id: Some(id),
                    trunk: host.trunk(),
                }),
                None => Err(format!("team {id} 不存在")),
            },
        }
    }
}

/// 命令处理器。
pub struct SessionCommandHandler {
    host: Arc<dyn SessionCommandHost>,
}

impl SessionCommandHandler {
    /// 用宿主接缝构造处理器。
    pub fn new(host: Arc<dyn SessionCommandHost>) -> Self {
        Self { host }
    }

    /// 处理一条命令；`None` = 不是本域。
    pub async fn handle(
        &self,
        name: &str,
        payload: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        if !COMMANDS.contains(&name) {
            return None;
        }
        let tagged = serde_json::json!({ name: payload });
        let cmd: BackendCommand = match serde_json::from_value(tagged) {
            Ok(cmd) => cmd,
            Err(error) => return Some(Err(format!("会话写命令载荷解析失败（{name}）: {error}"))),
        };
        Some(
            self.apply_command(cmd)
                .await
                .map(|()| serde_json::Value::Null),
        )
    }

    /// 强类型入口：宿主/测试已有一条 `BackendCommand` 时，省掉 JSON 往返。
    pub async fn apply_command(&self, cmd: BackendCommand) -> Result<(), String> {
        match cmd {
            BackendCommand::ClearHistory {
                team_id,
                session_id,
            } => {
                let session = require_session(session_id, "清理历史")?;
                let target = match Target::resolve_loaded(&self.host, team_id).await {
                    Ok(target) => target,
                    Err(message) => {
                        self.host.emit(BackendEvent::Error {
                            session_id: Some(session.clone()),
                            message: format!("会话历史不可用，已拒绝清理：{message}"),
                        });
                        return Ok(());
                    }
                };
                let _turn_guard = match self
                    .host
                    .quiesce_session(target.team_id.as_deref(), &session)
                    .await
                {
                    Ok(guard) => guard,
                    Err(message) => {
                        self.host.emit(BackendEvent::Notice {
                            team_id: target.team_id.clone(),
                            session_id: Some(session),
                            ok: false,
                            message: format!("未执行清理：{message}"),
                        });
                        return Ok(());
                    }
                };
                let result = target.trunk.clear_session(&session).await;
                self.host.forget_session_provider(&session);
                if let Err(error) = result {
                    self.after_write_result(
                        &target,
                        &session,
                        format!("内存历史已清理，但尚未落盘；重启可能恢复旧历史：{error}"),
                        false,
                    )
                    .await;
                    return Ok(());
                }
                tracing::info!(
                    team = ?target.team_id,
                    session = %session,
                    "session command: history cleared"
                );
                self.after_write(&target, &session, format!("会话历史已清理（{session}）"))
                    .await;
                Ok(())
            }
            BackendCommand::ArchiveHistory {
                team_id,
                session_id,
            } => {
                let session = require_session(session_id, "归档")?;
                let target = match Target::resolve_loaded(&self.host, team_id).await {
                    Ok(target) => target,
                    Err(message) => {
                        self.host.emit(BackendEvent::Error {
                            session_id: Some(session.clone()),
                            message: format!("会话历史不可用，已拒绝归档：{message}"),
                        });
                        return Ok(());
                    }
                };
                let _turn_guard = match self
                    .host
                    .quiesce_session(target.team_id.as_deref(), &session)
                    .await
                {
                    Ok(guard) => guard,
                    Err(message) => {
                        self.host.emit(BackendEvent::Notice {
                            team_id: target.team_id.clone(),
                            session_id: Some(session),
                            ok: false,
                            message: format!("未执行归档：{message}"),
                        });
                        return Ok(());
                    }
                };
                // 归档后清空是破坏性操作：旧 turn 已退出，守卫阻止新 turn 写入。
                let result = self.archive_with_progress(&target, &session).await;
                match result {
                    Ok(path) => {
                        self.after_write(&target, &session, format!("历史已归档：{path}"))
                            .await;
                    }
                    Err(error) => {
                        self.after_write_result(
                            &target,
                            &session,
                            format!("归档未完整完成：{error}"),
                            false,
                        )
                        .await;
                    }
                }
                Ok(())
            }
            BackendCommand::CompactHistory {
                team_id,
                session_id,
                keep_recent,
            } => {
                let session = require_session(session_id, "压缩")?;
                // 与旧实现同一区间：默认保留 40 条，硬边界 10..=500。
                let keep = keep_recent.unwrap_or(40).clamp(10, 500);
                // 压缩同样是**会话写操作**：目标必须先加载。加载失败时
                // 错误可见、不进入压缩（压空内存 / 写不出去都会"报成功却
                // 什么都没发生"）。team 解析也在这里做（失败回 `team 不存在`）。
                let target = match Target::resolve_loaded(&self.host, team_id.clone()).await {
                    Ok(target) => target,
                    Err(message) => {
                        self.host.emit(BackendEvent::Error {
                            session_id: Some(session.clone()),
                            message: format!("会话历史不可用，已拒绝压缩：{message}"),
                        });
                        return Ok(());
                    }
                };
                // 压缩要 provider + 策略模块 → 委托宿主（见模块注释的表）。
                let result = self.host.compact(team_id, &session, keep).await;
                match result {
                    Ok(message) => {
                        self.after_write(&target, &session, message).await;
                    }
                    Err(message) => {
                        self.host.emit(BackendEvent::Error {
                            session_id: Some(session),
                            message,
                        });
                    }
                }
                Ok(())
            }
            other => Err(format!(
                "session 命令域不处理 {:?}（注册关系与命令泵不一致）",
                other.kind_name()
            )),
        }
    }

    /// 写操作之后的闭环：重发**实际剩余**的时间线 + 该会话的快照 + 回执。
    ///
    /// 为什么必须重发真实剩余条目而不是空列表（2026-10 用户报障）：面板把
    /// "空全量"当权威数据，会清掉**整个本地聊天缓存**（含未被清理的其他
    /// 会话），本地游标还被推进到最新、刷新也拉不回来。
    async fn after_write(&self, target: &Target, session: &str, notice: String) {
        self.after_write_result(target, session, notice, true).await;
    }

    /// 即使保存失败也刷新实际内存状态，不能发出成功回执。
    async fn after_write_result(&self, target: &Target, session: &str, notice: String, ok: bool) {
        let messages = elide_reasoning_for_wire(target.trunk.timeline_snapshot());
        self.host.emit(BackendEvent::TrunkTimeline {
            messages,
            seq: target.trunk.timeline_seq(),
            team_id: target.team_id.clone(),
            full: true,
            has_more: false,
        });
        // 快照必须发给**被清理的会话**：发 `None` 会落到本地 TUI 会话，
        // 当前窗口的上下文面板因此不刷新（2026-10 作用域普查）。
        self.host.emit_context_snapshot(session).await;
        self.host.emit(BackendEvent::Notice {
            team_id: target.team_id.clone(),
            session_id: Some(session.to_string()),
            ok,
            message: notice,
        });
    }

    /// 归档并上报进度（`archive` → `done` / `failed`）。
    ///
    /// 进度事件是给**面板进度条**的：归档单会话也可能要写几十 MB 快照，
    /// 没有进度看起来像卡死。失败路径自己发一次终态，否则进度条永远转。
    async fn archive_with_progress(
        &self,
        target: &Target,
        session: &str,
    ) -> Result<String, String> {
        let emit = |phase: CompactionPhase, done, total, message: String| {
            self.host.emit(BackendEvent::CompactionProgress {
                team_id: target.team_id.clone(),
                session_id: Some(session.to_string()),
                phase: phase.as_str().to_string(),
                done,
                total,
                sessions: 1,
                message,
            });
        };
        emit(
            CompactionPhase::Archive,
            0,
            0,
            format!("正在归档会话 {session} 的快照…"),
        );
        let result = target.trunk.archive_session(session).await;
        match &result {
            Ok(path) => emit(CompactionPhase::Done, 1, 1, format!("历史已归档：{path}")),
            Err(error) => emit(CompactionPhase::Failed, 0, 0, format!("归档失败：{error}")),
        }
        result
    }
}

/// `session_id` 校验：缺失 = 拒绝（fail-closed，见模块注释）。
fn require_session(session_id: Option<String>, verb: &str) -> Result<String, String> {
    session_id.filter(|s| !s.is_empty()).ok_or_else(|| {
        format!("{verb}需要指定会话（session_id 缺失——客户端过旧，请升级面板后重试）")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 记录调用的假宿主。
    ///
    /// `trunk` 是**同一个** Arc（不再是每次新造）：坏档用例要预先给 store
    /// 设好持久化路径并触发 Failed，命令必须打在同一份 store 上才看得见。
    struct FakeHost {
        events: Mutex<Vec<BackendEvent>>,
        cleared_sessions: Mutex<Vec<String>>,
        snapshots: Mutex<Vec<String>>,
        compact_calls: Mutex<Vec<(Option<String>, String, usize)>>,
        compact_result: Mutex<Option<Result<String, String>>>,
        trunk: Arc<TrunkStore>,
        quiesce_error: Option<String>,
    }

    impl Default for FakeHost {
        fn default() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                cleared_sessions: Mutex::new(Vec::new()),
                snapshots: Mutex::new(Vec::new()),
                compact_calls: Mutex::new(Vec::new()),
                compact_result: Mutex::new(None),
                trunk: Arc::new(TrunkStore::new(1000)),
                quiesce_error: None,
            }
        }
    }

    impl SessionCommandHost for FakeHost {
        fn self_team_id(&self) -> Option<String> {
            Some("self".into())
        }
        fn resolve_trunk(&self, _team_id: &str) -> Option<Arc<TrunkStore>> {
            None
        }
        fn trunk(&self) -> Arc<TrunkStore> {
            Arc::clone(&self.trunk)
        }
        fn forget_session_provider(&self, session_id: &str) {
            self.cleared_sessions
                .lock()
                .unwrap()
                .push(session_id.to_string());
        }
        fn quiesce_session<'a>(
            &'a self,
            _team_id: Option<&'a str>,
            _session_id: &'a str,
        ) -> BoxFuture<'a, Result<Box<dyn Send>, String>> {
            Box::pin(async move {
                match &self.quiesce_error {
                    Some(error) => Err(error.clone()),
                    None => Ok(Box::new(()) as Box<dyn Send>),
                }
            })
        }
        fn emit(&self, event: BackendEvent) {
            self.events.lock().unwrap().push(event);
        }
        fn emit_context_snapshot<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()> {
            self.snapshots.lock().unwrap().push(session_id.to_string());
            Box::pin(async {})
        }
        fn compact<'a>(
            &'a self,
            team_id: Option<String>,
            session_id: &'a str,
            keep_recent: usize,
        ) -> BoxFuture<'a, Result<String, String>> {
            self.compact_calls
                .lock()
                .unwrap()
                .push((team_id, session_id.to_string(), keep_recent));
            let result = self
                .compact_result
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| Ok("compacted".into()));
            Box::pin(async move { result })
        }
    }

    fn handler() -> (SessionCommandHandler, Arc<FakeHost>) {
        let host = Arc::new(FakeHost::default());
        (SessionCommandHandler::new(host.clone()), host)
    }

    /// 会话档坏掉时，写命令必须**失败可见**（Error 事件 + 不执行清理），
    /// 不能"报成功却什么都没发生"。管理修复出口（retry/recover）不经过这里，
    /// 不会被入口门禁停掉。
    #[tokio::test]
    async fn corrupt_session_file_refuses_write_command_with_visible_error() {
        let path = std::env::temp_dir().join(format!(
            "echo-session-cmd-corrupt-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "{ 坏档").unwrap();

        let trunk = Arc::new(TrunkStore::new(1000));
        trunk.set_persist_path(&path);
        let host = Arc::new(FakeHost {
            trunk: Arc::clone(&trunk),
            ..Default::default()
        });
        let handler = SessionCommandHandler::new(host.clone());

        handler
            .handle(
                "ClearHistory",
                serde_json::json!({ "team_id": null, "session_id": "s1" }),
            )
            .await
            .expect("命令属于本域")
            .expect("加载失败走事件回执，不把命令本身当异常");
        assert!(
            host.cleared_sessions.lock().unwrap().is_empty(),
            "加载失败时不得执行清理（那会报成功却什么都没清）"
        );
        let events = host.events.lock().unwrap();
        let error = events
            .iter()
            .find_map(|event| match event {
                BackendEvent::Error {
                    session_id,
                    message,
                } => Some((session_id.clone(), message.clone())),
                _ => None,
            })
            .expect("失败必须对用户可见（Error 事件）");
        assert_eq!(error.0.as_deref(), Some("s1"), "错误要能定位到会话");
        assert!(error.1.contains("会话历史不可用"), "{}", error.1);
        drop(events);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn non_domain_command_is_not_claimed() {
        let (handler, _) = handler();
        assert!(handler
            .handle("SendMessage", serde_json::json!({}))
            .await
            .is_none());
    }

    /// 缺 session_id = 报错，**不**降级成"整个 persona"。
    #[tokio::test]
    async fn missing_session_is_refused_not_widened() {
        let (handler, host) = handler();
        let out = handler
            .handle(
                "ClearHistory",
                serde_json::json!({ "team_id": null, "session_id": null }),
            )
            .await
            .expect("claimed");
        assert!(out.is_err(), "缺 session 必须报错: {out:?}");
        assert!(host.cleared_sessions.lock().unwrap().is_empty());
    }

    /// 清理走：清会话 → 忘覆盖 → 重发时间线 → 快照 → 回执。
    #[tokio::test]
    async fn clear_emits_timeline_snapshot_then_notice() {
        let (handler, host) = handler();
        handler
            .handle(
                "ClearHistory",
                serde_json::json!({ "team_id": null, "session_id": "s1" }),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*host.cleared_sessions.lock().unwrap(), vec!["s1"]);
        assert_eq!(*host.snapshots.lock().unwrap(), vec!["s1"]);
        let events = host.events.lock().unwrap();
        assert_eq!(events.len(), 2, "时间线 + 回执: {events:?}");
        assert!(matches!(events[0], BackendEvent::TrunkTimeline { .. }));
        assert!(matches!(events[1], BackendEvent::Notice { ok: true, .. }));
    }

    /// 归档必须**上报进度**：`archive` 起手一条、终态（`done` / `failed`）一条。
    ///
    /// 这条锁的是面板进度条的数据源（2026-11 死代码清理时发现归档路径零测试
    /// 覆盖）。空 trunk 走 `Err("没有可归档的历史")`——顺带验证失败路径也发
    /// 终态，否则面板的进度条会永远转着（用户以为还在跑）。
    #[tokio::test]
    async fn archive_reports_progress_and_a_terminal_phase() {
        let (handler, host) = handler();
        handler
            .handle(
                "ArchiveHistory",
                serde_json::json!({ "team_id": null, "session_id": "s1" }),
            )
            .await
            .expect("claimed")
            .expect("归档失败走事件回执，不进 Err");
        let events = host.events.lock().unwrap();
        let phases: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                BackendEvent::CompactionProgress { phase, .. } => Some(phase.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            phases.first().copied(),
            Some("archive"),
            "起手必须有 archive 进度（面板据此起进度条）: {events:?}"
        );
        assert!(
            matches!(phases.last().copied(), Some("done" | "failed")),
            "终态必须是 done 或 failed，否则进度条永远转: {phases:?}"
        );
        // 空 trunk → 失败终态；随后还有一条 ok=false 的回执
        assert_eq!(phases.last().copied(), Some("failed"), "{phases:?}");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, BackendEvent::Notice { ok: false, .. })),
            "失败要有回执: {events:?}"
        );
    }

    /// 压缩保留区间与旧实现一致：默认 40，夹在 10..=500。
    #[tokio::test]
    async fn compact_clamps_keep_recent_like_before() {
        let (handler, host) = handler();
        for (given, want) in [
            (serde_json::Value::Null, 40usize),
            (serde_json::json!(3), 10),
            (serde_json::json!(999), 500),
        ] {
            handler
                .handle(
                    "CompactHistory",
                    serde_json::json!({
                        "team_id": null,
                        "session_id": "s1",
                        "keep_recent": given,
                    }),
                )
                .await
                .unwrap()
                .unwrap();
            let calls = host.compact_calls.lock().unwrap();
            assert_eq!(calls.last().unwrap().2, want);
        }
    }

    #[tokio::test]
    async fn quiesce_failure_refuses_clear_and_archive_without_mutation() {
        use echo_session::{SessionEvent, UserMessage};
        for command in ["ClearHistory", "ArchiveHistory"] {
            let host = Arc::new(FakeHost {
                quiesce_error: Some("旧 turn 尚未退出".into()),
                ..Default::default()
            });
            host.trunk
                .append_event(SessionEvent::UserMessage(UserMessage {
                    content: "must-survive".into(),
                    timestamp: 1,
                    message_sequence: None,
                    source: None,
                    images: vec![],
                    files: vec![],
                    session: Some("s1".into()),
                }));
            let before = host.trunk.event_log();
            let handler = SessionCommandHandler::new(host.clone());
            handler
                .handle(command, serde_json::json!({"session_id":"s1"}))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(host.trunk.event_log(), before);
            assert!(host.cleared_sessions.lock().unwrap().is_empty());
            let events = host.events.lock().unwrap();
            assert!(events
                .iter()
                .any(|e| matches!(e, BackendEvent::Notice { ok: false, .. })));
            assert!(!events
                .iter()
                .any(|e| matches!(e, BackendEvent::Notice { ok: true, .. })));
        }
    }

    #[tokio::test]
    async fn clear_save_failure_never_reports_success() {
        use echo_session::{SessionEvent, UserMessage};
        let root = std::env::temp_dir().join(format!(
            "echo-clear-save-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("sessions.json");
        let trunk = Arc::new(TrunkStore::new(1000));
        trunk.set_persist_path(&path);
        trunk.append_event(SessionEvent::UserMessage(UserMessage {
            content: "durable-before-clear".into(),
            timestamp: 1,
            message_sequence: None,
            source: None,
            images: vec![],
            files: vec![],
            session: Some("s1".into()),
        }));
        trunk.save_now_checked().await.unwrap();
        let original = std::fs::read(&path).unwrap();
        std::fs::create_dir(root.join("sessions.json.tmp")).unwrap();
        let host = Arc::new(FakeHost {
            trunk: Arc::clone(&trunk),
            ..Default::default()
        });
        let handler = SessionCommandHandler::new(host.clone());
        handler
            .handle("ClearHistory", serde_json::json!({"session_id":"s1"}))
            .await
            .unwrap()
            .unwrap();
        let events = host.events.lock().unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, BackendEvent::Notice { ok: true, .. })),
            "save failed but success reported: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BackendEvent::Notice { ok: false, .. })),
            "failure must be visible"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BackendEvent::TrunkTimeline { .. })),
            "refresh actual in-memory state even when save fails"
        );
        assert!(trunk.is_dirty());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 压缩失败 = 一条 Error 事件，不发时间线/回执。
    #[tokio::test]
    async fn compact_failure_emits_only_an_error() {
        let (handler, host) = handler();
        *host.compact_result.lock().unwrap() = Some(Err("历史不足".into()));
        handler
            .handle(
                "CompactHistory",
                serde_json::json!({ "team_id": null, "session_id": "s1", "keep_recent": 40 }),
            )
            .await
            .unwrap()
            .unwrap();
        let events = host.events.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(events[0], BackendEvent::Error { .. }));
    }
}
