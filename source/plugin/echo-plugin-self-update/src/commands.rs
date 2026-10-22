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

/// systemd 作用域（部署形状）：决定用 `systemctl` 还是 `systemctl --user`。
///
/// 这两条命令连的是**两套互不可见的 manager**：单元装在
/// `~/.config/systemd/user` 就归当前用户的 `systemd --user` 管，装在
/// `/etc/systemd/system` 就归 PID 1 管。敲错不会报"权限不足"，而是
/// `Unit ... not found` ——在自更新这里表现为"按钮点了只弹一个错"，服务
/// 完全没动。
///
/// 取值来源：`install.sh` 渲染单元时注入的 `ECHO_SERVICE_SCOPE`
/// （见 `packaging/systemd/*.service.in`）。这是唯一可信来源——**不用运行时
/// 探测**：两套单元同时存在（旧 system 装法残留 + 新 user 装法）时探测必然
/// 猜错一个。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceScope {
    /// 用户管理器（`systemctl --user`），install.sh 的历史默认作用域。
    User,
    /// 系统管理器（裸 `systemctl`），单元装在 `/etc/systemd/system`。
    System,
}

impl ServiceScope {
    /// 读 `ECHO_SERVICE_SCOPE`；未设置或取值不认识时退回 `User`
    /// （install.sh 的历史默认作用域）。
    pub fn from_env() -> Self {
        match std::env::var("ECHO_SERVICE_SCOPE").as_deref() {
            Ok("system") => Self::System,
            _ => Self::User,
        }
    }

    /// 该作用域下 systemctl 需要的前缀参数。
    pub fn systemctl_scope_args(self) -> &'static [&'static str] {
        match self {
            Self::User => &["--user"],
            Self::System => &[],
        }
    }
}

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
    read_status_at(&status_path()).await
}

/// 读**指定**状态文件 → `SelfUpdateStatus` 事件（无文件 = `idle`）。
///
/// 路径可注入是测试隔离的前提：此前测试靠 `set_var("XDG_STATE_HOME")` 改变
/// 进程级环境变量，两个测试并行时互相覆盖 → 偶发失败（2026-10 修）。
pub async fn read_status_at(path: &std::path::Path) -> BackendEvent {
    let mut fields = std::collections::HashMap::new();
    if let Ok(text) = tokio::fs::read_to_string(path).await {
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
    start_update_in(host, ServiceScope::from_env()).await
}

/// 同上，作用域显式给出（测试与非常规部署用）。
pub async fn start_update_in(host: &dyn SelfUpdateHost, scope: ServiceScope) {
    let scope_args = scope.systemctl_scope_args();
    // 防重入：已有 update service 在跑就不重复启动。
    let check = tokio::process::Command::new("systemctl")
        .args(scope_args)
        .args(["is-active", "--quiet", UPDATE_UNIT])
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
        .args(scope_args)
        .args(["start", UPDATE_UNIT])
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
                message: format!(
                    "自更新启动失败（systemctl {} exit {s}）——请检查 {UPDATE_UNIT} 是否装在 {} 作用域",
                    if scope_args.is_empty() { "" } else { "--user" },
                    if scope_args.is_empty() { "system" } else { "user" },
                ),
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
    /// 状态文件覆盖（`None` = 按 `XDG_STATE_HOME` 解析）。
    ///
    /// 放在结构上而不是每次读环境变量：测试并行跑时环境变量是**进程级**
    /// 副作用，会互相覆盖（2026-10 偶发失败的根因）。
    status_file: Option<std::path::PathBuf>,
}

impl SelfUpdateHandler {
    /// 用宿主接缝构造处理器。
    pub fn new(host: Arc<dyn SelfUpdateHost>) -> Self {
        Self {
            host,
            status_file: None,
        }
    }

    /// 指定状态文件（测试 / 非常规部署布局用）。
    pub fn with_status_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.status_file = Some(path.into());
        self
    }

    /// 本次调用要读的状态文件路径。
    fn status_file(&self) -> std::path::PathBuf {
        self.status_file.clone().unwrap_or_else(status_path)
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
                self.host.emit(read_status_at(&self.status_file()).await);
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

    /// 指向**指定**状态文件的处理器（并行安全的测试隔离）。
    fn handler_at(path: impl Into<std::path::PathBuf>) -> (SelfUpdateHandler, Arc<FakeHost>) {
        let host = Arc::new(FakeHost::default());
        (
            SelfUpdateHandler::new(host.clone()).with_status_path(path),
            host,
        )
    }

    #[tokio::test]
    async fn foreign_commands_are_not_claimed() {
        let (h, host) = handler();
        assert!(h
            .handle("RequestState", serde_json::Value::Null)
            .await
            .is_none());
        assert!(host.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_without_file_is_idle() {
        // 指向一个必然不存在的状态文件——**注入路径**而不是改环境变量：
        // 并行测试下 `set_var` 会与同进程其它测试互相覆盖（2026-10 修）。
        let path = std::env::temp_dir()
            .join(format!("echo-su-none-{}", std::process::id()))
            .join("echo-agent-core/update-status");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        let (h, host) = handler_at(&path);
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
        let dir = std::env::temp_dir().join(format!("echo-su-fields-{}", std::process::id()));
        let path = dir.join("echo-agent-core");
        std::fs::create_dir_all(&path).unwrap();
        let status_file = path.join("update-status");
        std::fs::write(
            &status_file,
            "state=running\nphase=building\nrevision=abc123\nmessage=编译中\nupdated_at=2026-11-01T00:00:00Z\n",
        )
        .unwrap();
        let (h, host) = handler_at(&status_file);
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

    /// 作用域只影响 systemctl 的前缀参数；system 作用域不能带 `--user`
    /// （带了就落到另一套 manager 上，报 Unit not found）。
    #[test]
    fn scope_maps_to_systemctl_arguments() {
        assert_eq!(ServiceScope::User.systemctl_scope_args(), &["--user"]);
        assert!(
            ServiceScope::System.systemctl_scope_args().is_empty(),
            "system scope must invoke plain `systemctl`: --user addresses a different manager"
        );
    }

    /// 未设置/取值不认识时退回 user：install.sh 的历史默认作用域。
    /// `from_env` 读进程级环境，这里只断言"没有这个变量"时的行为（CI 不设它）。
    #[test]
    fn scope_defaults_to_user_without_env() {
        if std::env::var("ECHO_SERVICE_SCOPE").is_ok() {
            return; // 环境污染时跳过（并行测试不该改动进程级环境）
        }
        assert_eq!(ServiceScope::from_env(), ServiceScope::User);
    }
}
