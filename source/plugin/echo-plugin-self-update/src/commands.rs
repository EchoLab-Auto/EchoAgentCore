//! 自更新命令域：`RequestSelfUpdate` / `RequestSelfUpdateStatus`。
//!
//! ## 为什么这两条命令归插件
//!
//! 它们此前住在组合根 `federation_rt.rs`（约 120 行）：触发
//! `echo-agent-core-update.service`、读 `~/.local/state/echo-agent-core/update-status`。
//! 那是**部署形状**——systemd 单元名、状态文件的 `key=value` 字段、什么算
//! "进行中"。这些东西换一个部署方式（容器 / 别的 init 系统）就该换掉，正是
//! 插件该有的样子；而组合根本不该知道"updater 单元叫什么"。
//!
//! ## 顺手修掉的一个死路
//!
//! 两条命令此前在 `HOST_RESERVED_COMMANDS` 里（"插件不得接管的自我管理面"），
//! 于是命令泵**跳过插件分发**、落进宿主的兜底分支，而兜底分支的文案是
//! "联邦命令由 echo-agent.federation 插件处理，但该插件未启用"——**即使联邦
//! 插件正常运行**。实现（`ports.command_handler` → `handle_federation_command`）
//! 只有经插件分发才走得到，两条命令因此必失败。
//!
//! 现在它们不 reserved：本插件是内置插件、启动期即挂载，认领这两条命令；
//! 未挂载时宿主兜底会明确说"自更新插件未启用"。
//!
//! ## 与联邦的独立性
//!
//! 自更新曾与联邦命令共用入口，于是"联邦关闭"会连带把自更新报成"联邦未
//! 启用"。现在两件事在**两个插件**里，互不牵连。

use std::sync::Arc;

use echo_protocol::{BackendCommand, BackendEvent};

/// 本插件处理的命令名。
pub const COMMANDS: [&str; 2] = ["RequestSelfUpdate", "RequestSelfUpdateStatus"];

/// updater 单元名（部署形状；换部署方式时只改这里）。
pub const UPDATE_UNIT: &str = "echo-agent-core-update.service";

/// 宿主接缝：本域只需要一个事件出口。
///
/// 刻意**不**给"读配置 / 拿句柄"之类的能力——自更新的全部知识（单元名、
/// 状态文件、重入判定）都在本插件里。
pub trait SelfUpdateHost: Send + Sync {
    /// 事件出口（进程级汇聚点：任何人格发都到同一处）。
    fn emit(&self, event: BackendEvent);
}

/// 自更新状态文件路径（与 `install.sh` / `update.sh` 布局一致）。
pub fn status_path() -> std::path::PathBuf {
    let state_home = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state")
        });
    state_home.join("echo-agent-core/update-status")
}

/// 读状态文件 → `SelfUpdateStatus` 事件（无文件 = `idle`）。
pub async fn read_status() -> BackendEvent {
    let path = status_path();
    let mut fields = std::collections::HashMap::new();
    if let Ok(text) = tokio::fs::read_to_string(&path).await {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                fields.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    BackendEvent::SelfUpdateStatus {
        state: fields
            .get("state")
            .cloned()
            .unwrap_or_else(|| "idle".into()),
        phase: fields.get("phase").cloned(),
        revision: fields.get("revision").cloned(),
        message: fields.get("message").cloned(),
        updated_at: fields.get("updated_at").cloned(),
    }
}

/// 触发自更新：启动 oneshot 单元（它脱离本进程执行"构建 → 替换 → 重启"，
/// agent 自杀不了它）。Graceful Drain 由 Core 的 SIGTERM 处理保证（当前回复
/// 完整生成后再退）。
pub async fn start_update(host: &dyn SelfUpdateHost) {
    // 防重入：已有 update service 在跑就不重复启动。
    let check = tokio::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", UPDATE_UNIT])
        .status()
        .await;
    if matches!(check, Ok(s) if s.success()) {
        host.emit(BackendEvent::Error {
            session_id: None,
            message: format!("自更新已在进行中（{UPDATE_UNIT} active）"),
        });
        return;
    }
    let result = tokio::process::Command::new("systemctl")
        .args(["--user", "start", UPDATE_UNIT])
        .status()
        .await;
    match result {
        Ok(s) if s.success() => {
            // 受理回执：状态文件很快会被 updater 写 running——前端轮询
            // `RequestSelfUpdateStatus` 观察进度；看到 restarting 即停止轮询
            // （服务即将重启，后续验证属新一轮）。
            host.emit(read_status().await);
        }
        Ok(s) => {
            host.emit(BackendEvent::Error {
                session_id: None,
                message: format!("自更新启动失败（systemctl exit {s}）——请检查 {UPDATE_UNIT}"),
            });
        }
        Err(e) => {
            host.emit(BackendEvent::Error {
                session_id: None,
                message: format!("自更新启动失败: {e}"),
            });
        }
    }
}

/// 命令处理器。
pub struct SelfUpdateHandler {
    host: Arc<dyn SelfUpdateHost>,
}

impl SelfUpdateHandler {
    /// 用宿主接缝构造处理器。
    pub fn new(host: Arc<dyn SelfUpdateHost>) -> Self {
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
            Err(error) => {
                return Some(Err(format!(
                    "self-update 命令载荷解析失败（{name}）: {error}"
                )))
            }
        };
        Some(self.apply(cmd).await.map(|()| serde_json::Value::Null))
    }

    async fn apply(&self, cmd: BackendCommand) -> Result<(), String> {
        match cmd {
            BackendCommand::RequestSelfUpdate => {
                start_update(self.host.as_ref()).await;
                Ok(())
            }
            BackendCommand::RequestSelfUpdateStatus => {
                self.host.emit(read_status().await);
                Ok(())
            }
            other => Err(format!(
                "self-update 命令域不处理 {:?}（注册关系与命令泵不一致）",
                other.kind_name()
            )),
        }
    }
}

/// 事件出口的**最小实现**（组合根只需要把 `EventSink` 包一层）。
pub struct SinkHost {
    sink: Arc<dyn Fn(BackendEvent) + Send + Sync>,
}

impl SinkHost {
    /// 用一个进程级事件出口构造。
    pub fn new(sink: Arc<dyn Fn(BackendEvent) + Send + Sync>) -> Self {
        Self { sink }
    }
}

impl SelfUpdateHost for SinkHost {
    fn emit(&self, event: BackendEvent) {
        (self.sink)(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeHost {
        events: Mutex<Vec<BackendEvent>>,
    }

    impl SelfUpdateHost for FakeHost {
        fn emit(&self, event: BackendEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn handler() -> (SelfUpdateHandler, Arc<FakeHost>) {
        let host = Arc::new(FakeHost::default());
        (SelfUpdateHandler::new(host.clone()), host)
    }

    #[tokio::test]
    async fn foreign_commands_are_not_claimed() {
        let (h, host) = handler();
        assert!(h.handle("RequestState", serde_json::Value::Null).await.is_none());
        assert!(host.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_without_file_is_idle() {
        // 指向一个必然不存在的状态目录，避免读到开发机上的真实状态。
        let dir = std::env::temp_dir().join(format!("echo-su-none-{}", std::process::id()));
        std::env::set_var("XDG_STATE_HOME", &dir);
        let (h, host) = handler();
        h.handle("RequestSelfUpdateStatus", serde_json::Value::Null)
            .await
            .unwrap()
            .unwrap();
        let events = host.events.lock().unwrap();
        match &events[0] {
            BackendEvent::SelfUpdateStatus { state, phase, .. } => {
                assert_eq!(state, "idle");
                assert!(phase.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_file_fields_are_projected() {
        let dir = std::env::temp_dir().join(format!("echo-su-{}", std::process::id()));
        let path = dir.join("echo-agent-core");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("update-status"),
            "state=running\nphase=building\nrevision=abc123\nmessage=编译中\nupdated_at=2026-11-01T00:00:00Z\n",
        )
        .unwrap();
        std::env::set_var("XDG_STATE_HOME", &dir);
        let (h, host) = handler();
        h.handle("RequestSelfUpdateStatus", serde_json::Value::Null)
            .await
            .unwrap()
            .unwrap();
        let events = host.events.lock().unwrap();
        match &events[0] {
            BackendEvent::SelfUpdateStatus {
                state,
                phase,
                revision,
                message,
                updated_at,
            } => {
                assert_eq!(state, "running");
                assert_eq!(phase.as_deref(), Some("building"));
                assert_eq!(revision.as_deref(), Some("abc123"));
                assert_eq!(message.as_deref(), Some("编译中"));
                assert!(updated_at.is_some());
            }
            other => panic!("unexpected: {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_unit_is_the_installed_oneshot_unit() {
        assert_eq!(UPDATE_UNIT, "echo-agent-core-update.service");
    }
}
