//! Shell 命令域：4 条 `*Shell*` 命令由本插件处理。
//!
//! ## 为什么 Shell 命令归本插件
//!
//! `shell_start` / `shell_exec` / `shell_stop` **工具**本来就住在这里
//! （`shell_tools.rs`）。面板侧的会话列表 / 启停与模型侧是同一件事的两个
//! 入口——把它们分开放在两个 crate 里，等于让"Shell 会话"这个概念横跨
//! 插件边界。现在两侧同一个 crate，宿主的 `apply_command` 只在落内置 match
//! **之前**问一次插件（`PluginHost::dispatch_command`），命中就返回。
//!
//! ## 宿主接缝 = 三项能力
//!
//! | 方法 | 宿主实现 |
//! |---|---|
//! | [`ShellCommandHost::manager`] | 进程级 `ShellHandle`（按 `team_id` 分桶） |
//! | [`ShellCommandHost::emit`] | `Agent::emit`（含 team 标注与插件事件回流） |
//! | [`ShellCommandHost::team_id`] | 本 persona 的 id（新会话归属） |
//!
//! 逐条对着原实现里的 `self.xxx()` 调的——**多一项都意味着插件多知道一件
//! 宿主内部结构**。`shell_emit_for_self` 那种"给 shell 的进度回调"由宿主
//! 在 `manager()` 之外单独给（[`ShellCommandHost::progress`]），因为它是
//! `echo-shell` 的类型，插件只是转交。

use std::sync::Arc;

use echo_protocol::{BackendCommand, BackendEvent};
use echo_shell::{ShellEmit, ShellManager};

/// 本插件处理的命令名（面板探测与文档用；`dispatch_command` 按名字匹配）。
pub const COMMANDS: [&str; 4] = [
    "RequestShellSessions",
    "ShellStart",
    "ShellExec",
    "ShellStop",
];

/// 宿主接缝：Shell 命令域需要的三件事 + 一个进度回调工厂。
pub trait ShellCommandHost: Send + Sync {
    /// Shell 会话管理器（None = 未接线；命令回可读错误）。
    fn manager(&self) -> Option<Arc<ShellManager>>;
    /// 事件出口（宿主 `emit`：team 标注、事件总线、插件回流都由它负责）。
    fn emit(&self, event: BackendEvent);
    /// 执行命令的 persona id（新会话归属）。`None` = 无归属。
    fn team_id(&self) -> Option<String>;
    /// 给 `echo-shell` 的进度回调（host `shell_emit_for_self` 的同款）。
    fn progress(&self) -> ShellEmit;
}

/// 命令处理器：把"名字 + 载荷"还原成强类型命令后逐条处理。
///
/// 依赖 `echo-protocol`（命令 / 事件词汇的正确归属，与 workspace 插件同款
/// 口径）：插件与宿主共享的是**协议**，不是宿主内部类型。
pub struct ShellCommandHandler {
    host: Arc<dyn ShellCommandHost>,
}

impl ShellCommandHandler {
    pub fn new(host: Arc<dyn ShellCommandHost>) -> Self {
        Self { host }
    }

    /// 处理一条命令；返回 `None` = 不是本域的命令（交给下一个插件/宿主）。
    pub async fn handle(
        &self,
        name: &str,
        payload: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        if !COMMANDS.contains(&name) {
            return None;
        }
        // 名字 → 强类型：`payload` 是**剥掉外层 tag** 的载荷，套回
        // `{ name: payload }` 再反序列化（与 workspace 插件同一手法）。
        // 协议变体改名时这里编译失败（而不是运行期静默不匹配）。
        let tagged = serde_json::json!({ name: payload });
        let cmd: BackendCommand = match serde_json::from_value(tagged) {
            Ok(cmd) => cmd,
            Err(error) => {
                return Some(Err(format!(
                    "shell 命令载荷解析失败（{name}）: {error}"
                )))
            }
        };
        Some(self.apply(cmd).await.map(|()| serde_json::Value::Null))
    }

    async fn apply(&self, cmd: BackendCommand) -> Result<(), String> {
        match cmd {
            BackendCommand::RequestShellSessions { team_id } => {
                let sessions = self
                    .host
                    .manager()
                    .map(|m| m.list(team_id.as_deref()))
                    .unwrap_or_default();
                self.host.emit(BackendEvent::ShellSessionsList {
                    sessions,
                    team_id,
                });
                Ok(())
            }
            BackendCommand::ShellStart { workdir } => {
                let Some(manager) = self.host.manager() else {
                    self.notice("shell manager unavailable");
                    return Ok(());
                };
                let emit = self.host.progress();
                let team_id = self.host.team_id();
                match manager.start(workdir, team_id, &emit).await {
                    Ok(info) => {
                        self.host
                            .emit(BackendEvent::ShellSessionStarted { session: info.clone() });
                        self.notice(format!("shell session started: {}", info.session_id));
                    }
                    Err(error) => self.notice(format!("shell start failed: {error}")),
                }
                Ok(())
            }
            BackendCommand::ShellExec {
                session_id,
                command,
                timeout_secs,
            } => {
                let Some(manager) = self.host.manager() else {
                    self.notice("shell manager unavailable");
                    return Ok(());
                };
                let emit = self.host.progress();
                match manager
                    .exec(&session_id, &command, timeout_secs, &emit)
                    .await
                {
                    Ok((output, _success, _timed_out)) => self.notice(format!(
                        "shell exec done ({session_id}): {} chars",
                        output.chars().count()
                    )),
                    Err(error) => self.notice(format!("shell exec failed: {error}")),
                }
                Ok(())
            }
            BackendCommand::ShellStop { session_id } => {
                let Some(manager) = self.host.manager() else {
                    self.notice("shell manager unavailable");
                    return Ok(());
                };
                let emit = self.host.progress();
                match manager.stop(&session_id, &emit).await {
                    Ok(()) => self.notice(format!("shell session {session_id} stopped")),
                    Err(error) => self.notice(format!("shell stop failed: {error}")),
                }
                Ok(())
            }
            // 名字在 COMMANDS 里但没有分支：契约破了（加命令忘加处理）。
            other => Err(format!(
                "shell 命令域声明了 {} 但处理器没有对应分支",
                other.kind_name()
            )),
        }
    }

    fn notice(&self, message: impl Into<String>) {
        self.host.emit(BackendEvent::Error {
            session_id: None,
            message: message.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_protocol::BackendEvent;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeHost {
        events: Mutex<Vec<BackendEvent>>,
        manager: Option<Arc<ShellManager>>,
    }

    impl ShellCommandHost for FakeHost {
        fn manager(&self) -> Option<Arc<ShellManager>> {
            self.manager.clone()
        }
        fn emit(&self, event: BackendEvent) {
            self.events.lock().unwrap().push(event);
        }
        fn team_id(&self) -> Option<String> {
            Some("t1".into())
        }
        fn progress(&self) -> ShellEmit {
            Arc::new(|_event| {})
        }
    }

    /// 返回（处理器，宿主句柄）——句柄留一份给断言用。
    fn handler(host: FakeHost) -> (ShellCommandHandler, Arc<FakeHost>) {
        let host = Arc::new(host);
        (ShellCommandHandler::new(host.clone()), host)
    }

    /// 未接线（无 manager）时给**明确**错误，不是静默成功。
    #[tokio::test]
    async fn without_a_manager_every_command_reports_unavailable() {
        let (h, host) = handler(FakeHost::default());
        for name in ["ShellStart", "ShellExec", "ShellStop"] {
            let payload = match name {
                "ShellStart" => serde_json::json!({"workdir": null}),
                "ShellExec" => {
                    serde_json::json!({"session_id": "s", "command": "ls", "timeout_secs": null})
                }
                _ => serde_json::json!({"session_id": "s"}),
            };
            h.handle(name, payload)
                .await
                .expect("本域命令必须被认领")
                .expect("未接线不是解析错误");
        }
        let events = host.events.lock().unwrap();
        assert_eq!(events.len(), 3, "三条命令各一条错误事件");
        for event in events.iter() {
            match event {
                BackendEvent::Error { message, .. } => {
                    assert!(message.contains("unavailable"), "{message}")
                }
                other => panic!("期望 Error 事件，得到 {other:?}"),
            }
        }
    }

    /// 会话列表：未接线时回**空表**（面板据此显示"没有会话"，不是报错）。
    #[tokio::test]
    async fn sessions_list_is_empty_but_emitted_when_unlinked() {
        let (h, host) = handler(FakeHost::default());
        h.handle("RequestShellSessions", serde_json::json!({"team_id": null}))
            .await
            .expect("本域命令")
            .expect("空表不是错误");
        let events = host.events.lock().unwrap();
        assert!(matches!(
            events.as_slice(),
            [BackendEvent::ShellSessionsList { sessions, .. }] if sessions.is_empty()
        ));
    }

    /// 不属于本域的命令返回 `None`（让下一个插件/宿主接手）。
    #[tokio::test]
    async fn foreign_commands_are_not_claimed() {
        let (h, _host) = handler(FakeHost::default());
        assert!(h
            .handle("RequestToolsList", serde_json::json!({}))
            .await
            .is_none());
    }

    /// 载荷形状不对：是**解析错误**（被认领但失败），不是静默放过。
    #[tokio::test]
    async fn malformed_payload_is_an_error_not_a_silent_pass() {
        let (h, _host) = handler(FakeHost::default());
        let outcome = h
            .handle("ShellStart", serde_json::json!(42))
            .await
            .expect("仍是本域命令");
        assert!(outcome.is_err(), "载荷不合法必须报错");
    }

    /// 多余字段是**宽容**的（协议演进时旧前端仍可用），不是错误。
    #[tokio::test]
    async fn unknown_extra_fields_are_tolerated() {
        let (h, _host) = handler(FakeHost::default());
        let outcome = h
            .handle("ShellStart", serde_json::json!({"workdir": null, "future": 1}))
            .await
            .expect("仍是本域命令");
        assert!(outcome.is_ok(), "多余字段不该让命令失败");
    }
}
