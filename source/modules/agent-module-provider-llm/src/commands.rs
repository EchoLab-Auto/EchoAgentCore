//! 供应商 / API 命令域：13 条命令由本模块处理（全模块化 S5）。
//!
//! ## 为什么这个命令域归本模块
//!
//! 池规则（`admin`）与 provider 工厂都在这里，**池的管理入口**也该在这里。
//! 此前 13 条命令的分支住在宿主 `agent/commands.rs`：宿主既知道"哪个 profile
//! 该激活"，又知道"切失败要回滚"，还知道"给面板的文案长什么样"——而它并不
//! 持有池的规则。搬过来之后：
//!
//! - 模块：命令名（注册关系）、载荷解析、事件形状、文案；
//! - 宿主：状态迁移的**原子动作**（读配置 → 调 `admin` → 写回 / 落盘 /
//!   重建 provider / 广播）。见 `echo-agent::agent::provider_host`。
//!
//! ## 与 `admin` 的分工
//!
//! `admin` 是**纯规则**（输入 `PoolState` 视图，输出 `PoolEffect` 列表）。
//! 本模块是**命令面**：把线命令翻译成宿主接缝调用。两者都不做副作用，
//! 副作用只有宿主能做（它持有 `AgentConfig` / provider / 事件出口）。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use echo_defs::api_profile::{ReasoningEffort, ThinkingMode};
use agent_module_management::protocol::{BackendCommand, BackendEvent};

/// 本模块处理的命令名（`RenameApi` 是**进程级**命令，归组合根，不在此列）。
pub const COMMANDS: [&str; 17] = [
    "SwitchModel",
    "SwitchProvider",
    "SetSystemPrompt",
    "RequestSystemPrompt",
    "UpdateApiConfig",
    "SwitchApi",
    "SetSessionApi",
    "ClearSessionApi",
    "RequestRemoteApiProfiles",
    "TestApi",
    "QueryApiBalance",
    "QueryApiMetrics",
    "DeleteApi",
    // ChatGPT 订阅登录（2026-11）：设置页直接发起，不再依赖 CLI。
    // 三条都在本模块——`chatgpt` 模块就在这里（登录流程 + 令牌落盘）。
    "StartChatGptLogin",
    "CancelChatGptLogin",
    "SubmitChatGptLoginCode",
    "RequestChatGptStatus",
];

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// `UpdateApiConfig` 的参数包。
///
/// 七个位置参数会让调用点读不懂（宿主侧同名方法的既有形状保留），所以命令面
/// 收成一个结构体——**字段语义与线契约一致**，空串 = "沿用当前值"。
#[derive(Debug, Clone, Default)]
pub struct ApiUpdate {
    /// 空 = 顶层默认配置。
    pub name: String,
    /// 供应商种类（`openai` / `deepseek` / …）。
    pub provider: String,
    /// 模型名。
    pub model: String,
    /// API base url。
    pub base_url: String,
    /// 空 = 保留原密钥（不覆盖）。
    pub api_key: String,
    /// 思考模式（`None` = 沿用当前值）。
    pub thinking: Option<ThinkingMode>,
    /// 推理强度（`None` = 沿用当前值）。
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// 宿主接缝：命令域需要而模块不该做的十三件事。
///
/// 每一项都是**状态迁移**（要 `AgentConfig` / provider 重建 / 落盘 / 事件
/// 出口 / 联邦查询），模块只负责"什么时候调、给什么参数、回什么文案"。
pub trait ProviderCommandHost: Send + Sync {
    /// 切模型（顶层 `[agent].model`）。
    fn switch_model<'a>(&'a self, model: &'a str) -> BoxFuture<'a, ()>;
    /// 切 provider（失败回滚到旧值）。
    fn switch_provider<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, ()>;
    /// 写系统提示词（宿主还要落盘到 `[modules.system_prompt]` 并失效缓存）。
    fn set_system_prompt<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, ()>;
    /// 读系统提示词（纯读，事件由模块发）。
    fn system_prompt(&self) -> BoxFuture<'_, String>;
    /// 新增 / 更新一条 profile 并激活。
    fn update_api<'a>(&'a self, update: ApiUpdate) -> BoxFuture<'a, ()>;
    /// 按名激活 profile（空 = 顶层默认）。
    fn switch_api<'a>(&'a self, name: &'a str) -> BoxFuture<'a, ()>;
    /// 会话级覆盖（空 profile = 清除）。
    fn set_session_api<'a>(&'a self, session_id: &'a str, profile: &'a str) -> BoxFuture<'a, ()>;
    /// 清除会话级覆盖。
    fn clear_session_api<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()>;
    /// 经联邦拉远端**脱敏**池：`Ok((active, profiles))` / `Err(原因)`。
    ///
    /// 两项都是**对端应答的原样 JSON**：那份形状由应答方的联邦查询处理器
    /// 决定（`federation_rt` 的 `ApiProfiles` 分支），本模块只把它装进
    /// `RemoteApiProfiles` 事件——不在这里解析、也不重建类型，于是"对端加了
    /// 一个字段"不会让本模块丢掉它。
    fn remote_api_profiles<'a>(
        &'a self,
        peer: &'a str,
    ) -> BoxFuture<'a, Result<(Option<serde_json::Value>, Vec<serde_json::Value>), String>>;
    /// 连通性自测（回 `ApiTestResult` 事件）。
    fn test_api<'a>(&'a self, name: &'a str) -> BoxFuture<'a, ()>;
    /// 账户余额（回 `ApiBalanceResult`）。
    fn query_api_balance<'a>(&'a self, name: &'a str) -> BoxFuture<'a, ()>;
    /// 本地积累指标（回 `ApiMetrics`）。
    fn query_api_metrics<'a>(&'a self, name: &'a str) -> BoxFuture<'a, ()>;
    /// 删除一条 profile（激活项删除后回落顶层默认）。
    fn delete_api<'a>(&'a self, name: &'a str) -> BoxFuture<'a, ()>;
    /// 事件出口。
    fn emit(&self, event: BackendEvent);
}

/// 命令处理器。
pub struct ProviderCommandHandler {
    host: Arc<dyn ProviderCommandHost>,
    /// 进行中的登录的取消开关。
    ///
    /// 只存"取消信号发送端"：登录任务拿接收端。用户在面板点取消 / 关掉弹层
    /// 时发一个信号，`select!` 让任务提前结束——**drop 掉 `PendingLogin`
    /// 会释放 1455 端口**，所以取消是"真的放掉"，不是只标个状态。不放掉的
    /// 后果很具体：用户取消后想重试，会撞上自己上一次登录占着的端口。
    login_cancel: std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    /// 手工提交授权码的投递端（局域网 / 远程部署的主要路径）。
    ///
    /// 与 `login_cancel` 分开存：两者生命周期不同——取消是一次性的（用完即弃），
    /// 而提交码在**用户粘贴之前**必须一直有效（他可能过一两分钟才复制完）。
    login_manual: std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
}

impl ProviderCommandHandler {
    /// 用宿主接缝构造处理器。
    pub fn new(host: Arc<dyn ProviderCommandHost>) -> Self {
        Self {
            host,
            login_cancel: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            login_manual: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        }
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
                return Some(Err(format!("provider 命令载荷解析失败（{name}）: {error}")))
            }
        };
        Some(self.apply(cmd).await.map(|()| serde_json::Value::Null))
    }

    async fn apply(&self, cmd: BackendCommand) -> Result<(), String> {
        match cmd {
            BackendCommand::SwitchModel { model } => {
                self.host.switch_model(&model).await;
                Ok(())
            }
            BackendCommand::SwitchProvider { provider } => {
                self.host.switch_provider(&provider).await;
                Ok(())
            }
            BackendCommand::SetSystemPrompt { prompt } => {
                self.host.set_system_prompt(&prompt).await;
                Ok(())
            }
            BackendCommand::RequestSystemPrompt => {
                let text = self.host.system_prompt().await;
                self.host.emit(BackendEvent::SystemPrompt { text });
                Ok(())
            }
            BackendCommand::UpdateApiConfig {
                name,
                provider,
                model,
                base_url,
                api_key,
                thinking,
                reasoning_effort,
            } => {
                self.host
                    .update_api(ApiUpdate {
                        name,
                        provider,
                        model,
                        base_url,
                        api_key,
                        thinking,
                        reasoning_effort,
                    })
                    .await;
                Ok(())
            }
            BackendCommand::SwitchApi { name } => {
                self.host.switch_api(&name).await;
                Ok(())
            }
            BackendCommand::SetSessionApi {
                session_id,
                profile,
                ..
            } => {
                // team_id 已由 `requires_explicit_team` 保证非空；宿主接缝按
                // 会话 id 定位（会话键里带归属），不必再传一遍。
                self.host.set_session_api(&session_id, &profile).await;
                Ok(())
            }
            BackendCommand::ClearSessionApi { session_id, .. } => {
                self.host.clear_session_api(&session_id).await;
                Ok(())
            }
            BackendCommand::RequestRemoteApiProfiles { peer } => {
                let (active, profiles, error) = match self.host.remote_api_profiles(&peer).await {
                    Ok((active, profiles)) => (active, profiles, None),
                    Err(message) => (None, Vec::new(), Some(message)),
                };
                self.host.emit(BackendEvent::RemoteApiProfiles {
                    peer,
                    active,
                    profiles,
                    error,
                });
                Ok(())
            }
            BackendCommand::TestApi { name } => {
                self.host.test_api(&name).await;
                Ok(())
            }
            BackendCommand::QueryApiBalance { name } => {
                self.host.query_api_balance(&name).await;
                Ok(())
            }
            BackendCommand::QueryApiMetrics { name } => {
                self.host.query_api_metrics(&name).await;
                Ok(())
            }
            BackendCommand::DeleteApi { name } => {
                self.host.delete_api(&name).await;
                Ok(())
            }
            BackendCommand::RequestChatGptStatus => {
                let status = crate::chatgpt::auth::status_snapshot();
                self.host.emit(status);
                Ok(())
            }
            BackendCommand::StartChatGptLogin => self.start_chatgpt_login().await,
            BackendCommand::SubmitChatGptLoginCode { input } => {
                let mut slot = self.login_manual.lock().await;
                match slot.take() {
                    Some(sender) => {
                        // 接收端已 drop = 登录任务已经结束（超时/取消/别处成功）。
                        // 这不是错误，但要说清楚——用户以为粘了就能成。
                        sender.send(input).map_err(|_| {
                            "登录已经结束（超时或被取消），请重新点「登录 ChatGPT」".to_string()
                        })
                    }
                    None => Err("当前没有进行中的登录：先点「登录 ChatGPT」拿到授权链接"
                        .to_string()),
                }
            }
            BackendCommand::CancelChatGptLogin => {
                let mut slot = self.login_cancel.lock().await;
                match slot.take() {
                    Some(cancel) => {
                        // 发送失败 = 任务已经自己结束了（成功 / 超时），
                        // 这不算错误：用户点取消时它恰好完成，是正常竞态。
                        let _ = cancel.send(());
                        Ok(())
                    }
                    None => Err("当前没有进行中的 ChatGPT 登录".into()),
                }
            }
            other => Err(format!(
                "provider 命令域不处理 {:?}（注册关系与命令泵不一致）",
                other.kind_name()
            )),
        }
    }
    /// 发起一次登录：预检 → 绑端口 → 回推 URL → 后台等回调。
    ///
    /// **立刻返回**：`handle` 不该挂 5 分钟。URL 一拿到就回推，等回调的部分
    /// 交给 spawn 出去的任务，面板据此能马上把 URL 显示给用户。
    async fn start_chatgpt_login(&self) -> Result<(), String> {
        use crate::chatgpt::auth;

        // 先占槽再干活：两个并发请求不能都通过"槽是空的"检查。
        // 通道在这一步就建好（而不是先放哨兵再换）：哨兵的接收端会被立刻
        // drop，那时取消请求会静默失败。
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        {
            let mut slot = self.login_cancel.lock().await;
            if slot.is_some() {
                return Err(
                    "已有 ChatGPT 登录在进行中：先在浏览器完成它，或点「取消」后重试".into(),
                );
            }
            *slot = Some(cancel_tx);
        }

        // 预检在绑端口**之前**：地区被拒时没必要占住 1455（codex CLI 也用
        // 这个端口）。失败也要清槽，否则接下来永远"已有登录在进行"。
        let fail = |error: String| async move {
            self.login_cancel.lock().await.take();
            self.login_manual.lock().await.take();
            self.host.emit(BackendEvent::ChatGptLoginResult {
                ok: false,
                email: String::new(),
                error: Some(error),
            });
        };
        if let Err(error) = auth::preflight().await {
            fail(error).await;
            return Ok(());
        }

        let pending = match auth::begin_login().await {
            Ok(pending) => pending,
            Err(error) => {
                fail(error).await;
                return Ok(());
            }
        };
        let url = pending.url.clone();
        self.host.emit(BackendEvent::ChatGptLoginPending {
            url,
            redirect_uri: auth::redirect_uri(),
        });

        // 手工提交授权码的通道：局域网 / 远程部署时**唯一**能走通的路
        // （回调固定打浏览器那台的 localhost:1455，Core 收不到）。
        let (manual_tx, manual_rx) = tokio::sync::oneshot::channel();
        {
            let mut slot = self.login_manual.lock().await;
            *slot = Some(manual_tx);
        }

        let host = Arc::clone(&self.host);
        let slot = Arc::clone(&self.login_cancel);
        let manual_slot = Arc::clone(&self.login_manual);
        tokio::spawn(async move {
            // 两条输入竞争：HTTP 回调（本机浏览器）与手工粘贴（远程浏览器）。
            // 取消单独 select 一层——它要**立刻**释放端口：drop 掉 pending 会把
            // listener 一起 drop，端口随之放掉，否则用户取消后重试会撞上自己。
            let outcome = tokio::select! {
                result = pending.wait_with_manual(manual_rx) => result,
                _ = cancel_rx => Err("登录已取消".to_string()),
            };
            // 无论成败都清两个槽：不清的话下一次登录会被告知"已有登录在进行"。
            slot.lock().await.take();
            manual_slot.lock().await.take();
            match outcome {
                Ok(auth) => host.emit(BackendEvent::ChatGptLoginResult {
                    ok: true,
                    email: auth.email,
                    error: None,
                }),
                Err(error) => host.emit(BackendEvent::ChatGptLoginResult {
                    ok: false,
                    email: String::new(),
                    error: Some(error),
                }),
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 记录调用顺序的假宿主：命令面搬到模块后，唯一可断言的就是"哪条命令
    /// 触发哪个接缝、发了哪个事件"——这正是归属关系的可测形状。
    #[derive(Default)]
    struct FakeHost {
        calls: Mutex<Vec<String>>,
        events: Mutex<Vec<BackendEvent>>,
        remote: Mutex<Option<Result<(Option<serde_json::Value>, Vec<serde_json::Value>), String>>>,
    }

    impl FakeHost {
        fn log(&self, entry: impl Into<String>) {
            self.calls.lock().unwrap().push(entry.into());
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn events(&self) -> Vec<BackendEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl ProviderCommandHost for FakeHost {
        fn switch_model<'a>(&'a self, model: &'a str) -> BoxFuture<'a, ()> {
            self.log(format!("switch_model:{model}"));
            Box::pin(async {})
        }
        fn switch_provider<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, ()> {
            self.log(format!("switch_provider:{provider}"));
            Box::pin(async {})
        }
        fn set_system_prompt<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, ()> {
            self.log(format!("set_system_prompt:{prompt}"));
            Box::pin(async {})
        }
        fn system_prompt(&self) -> BoxFuture<'_, String> {
            self.log("system_prompt");
            Box::pin(async { "PROMPT".to_string() })
        }
        fn update_api<'a>(&'a self, update: ApiUpdate) -> BoxFuture<'a, ()> {
            self.log(format!("update_api:{}", update.name));
            Box::pin(async {})
        }
        fn switch_api<'a>(&'a self, name: &'a str) -> BoxFuture<'a, ()> {
            self.log(format!("switch_api:{name}"));
            Box::pin(async {})
        }
        fn set_session_api<'a>(
            &'a self,
            session_id: &'a str,
            profile: &'a str,
        ) -> BoxFuture<'a, ()> {
            self.log(format!("set_session_api:{session_id}:{profile}"));
            Box::pin(async {})
        }
        fn clear_session_api<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()> {
            self.log(format!("clear_session_api:{session_id}"));
            Box::pin(async {})
        }
        fn remote_api_profiles<'a>(
            &'a self,
            peer: &'a str,
        ) -> BoxFuture<'a, Result<(Option<serde_json::Value>, Vec<serde_json::Value>), String>>
        {
            self.log(format!("remote_api_profiles:{peer}"));
            let result = self
                .remote
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| Ok((None, Vec::new())));
            Box::pin(async move { result })
        }
        fn test_api<'a>(&'a self, name: &'a str) -> BoxFuture<'a, ()> {
            self.log(format!("test_api:{name}"));
            Box::pin(async {})
        }
        fn query_api_balance<'a>(&'a self, name: &'a str) -> BoxFuture<'a, ()> {
            self.log(format!("query_api_balance:{name}"));
            Box::pin(async {})
        }
        fn query_api_metrics<'a>(&'a self, name: &'a str) -> BoxFuture<'a, ()> {
            self.log(format!("query_api_metrics:{name}"));
            Box::pin(async {})
        }
        fn delete_api<'a>(&'a self, name: &'a str) -> BoxFuture<'a, ()> {
            self.log(format!("delete_api:{name}"));
            Box::pin(async {})
        }
        fn emit(&self, event: BackendEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn handler() -> (ProviderCommandHandler, Arc<FakeHost>) {
        let host = Arc::new(FakeHost::default());
        (ProviderCommandHandler::new(host.clone()), host)
    }

    #[tokio::test]
    async fn non_domain_command_is_not_claimed() {
        let (h, host) = handler();
        let out = h
            .handle(
                "SendMessage",
                serde_json::json!({"session_id":"s","content":"x"}),
            )
            .await;
        assert!(out.is_none());
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn switch_model_reaches_the_seam() {
        let (h, host) = handler();
        h.handle("SwitchModel", serde_json::json!({"model":"kimi"}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(host.calls(), ["switch_model:kimi"]);
    }

    #[tokio::test]
    async fn system_prompt_is_read_here_event_emitted_here() {
        let (h, host) = handler();
        // 单元变体的线载荷是 `Null`（`payload_value()` 对无字段命令返回它）。
        h.handle("RequestSystemPrompt", serde_json::Value::Null)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(host.calls(), ["system_prompt"]);
        match &host.events()[0] {
            BackendEvent::SystemPrompt { text } => assert_eq!(text, "PROMPT"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn remote_profiles_failure_becomes_an_error_event() {
        let (h, host) = handler();
        *host.remote.lock().unwrap() = Some(Err("peer offline".into()));
        h.handle("RequestRemoteApiProfiles", serde_json::json!({"peer":"n1"}))
            .await
            .unwrap()
            .unwrap();
        match &host.events()[0] {
            BackendEvent::RemoteApiProfiles {
                peer,
                error,
                profiles,
                ..
            } => {
                assert_eq!(peer, "n1");
                assert_eq!(error.as_deref(), Some("peer offline"));
                assert!(profiles.is_empty());
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_payload_is_reported_not_silently_dropped() {
        let (h, _host) = handler();
        let out = h
            .handle("SwitchModel", serde_json::json!({"wrong":"field"}))
            .await
            .unwrap();
        assert!(out.is_err(), "畸形载荷必须回错误，不能静默");
    }

    #[tokio::test]
    async fn every_declared_command_is_dispatchable() {
        // 每条声明过的命令都必须能解析并落到某个接缝（防"声明了但 match
        // 漏了"）。这里排除 `StartChatGptLogin` / `CancelChatGptLogin`：
        // 前者会真去绑 1455 并打网络（CI 里必失败），后者在没有进行中的
        // 登录时**本来就该返回 Err**。两条单独覆盖，见下个用例。
        // 三条豁免：Start 会真打网络并绑 1455；Cancel/Submit 在没有进行中的
        // 登录时**本来就该返回 Err**（见各自的专门用例）。
        let exempt = [
            "StartChatGptLogin",
            "CancelChatGptLogin",
            "SubmitChatGptLoginCode",
        ];
        let (h, host) = handler();
        let payloads = serde_json::json!({
            "SwitchModel": {"model":"m"},
            "SwitchProvider": {"provider":"p"},
            "SetSystemPrompt": {"prompt":"x"},
            "RequestSystemPrompt": serde_json::Value::Null,
            "UpdateApiConfig": {"name":"n","provider":"p","model":"m","base_url":"","api_key":""},
            "SwitchApi": {"name":"n"},
            "SetSessionApi": {"team_id":"t","session_id":"s","profile":"n"},
            "ClearSessionApi": {"team_id":"t","session_id":"s"},
            "RequestRemoteApiProfiles": {"peer":"p"},
            "TestApi": {"name":"n"},
            "QueryApiBalance": {"name":"n"},
            "QueryApiMetrics": {"name":"n"},
            "DeleteApi": {"name":"n"},
            // 无载荷变体：线格式是裸字符串 `"RequestChatGptStatus"`，
            // payload 传 null 即"没有字段"。
            "RequestChatGptStatus": serde_json::Value::Null,
        });
        for name in COMMANDS {
            if exempt.contains(&name) {
                continue;
            }
            let payload = payloads[name].clone();
            let calls_before = host.calls().len();
            let events_before = host.events().len();
            let out = h.handle(name, payload).await;
            assert!(out.is_some(), "{name} 未被认领");
            assert!(out.unwrap().is_ok(), "{name} 解析失败");
            // **可观察效果**：要么调了接缝，要么发了事件。只断言 is_ok 会
            // 漏掉"解析通过但什么都没做"这种分支（`RequestChatGptStatus`
            // 就只发事件、不调接缝，所以不能按接缝调用数来数）。
            assert!(
                host.calls().len() > calls_before || host.events().len() > events_before,
                "{name} 解析通过但没有任何可观察效果"
            );
        }

        // 豁免的两条仍必须**已声明**且**线格式可解析**（它们若被改名或
        // 从枚举里删掉，这里会红）。
        for name in exempt {
            assert!(COMMANDS.contains(&name), "{name} 未声明");
            // 无载荷变体传 null；SubmitChatGptLoginCode 需要一个 input 字段。
            let payload = if name == "SubmitChatGptLoginCode" {
                serde_json::json!({ "input": "x" })
            } else {
                serde_json::Value::Null
            };
            let tagged = serde_json::json!({ name: payload });
            let parsed: Result<BackendCommand, _> = serde_json::from_value(tagged);
            assert!(parsed.is_ok(), "{name} 线格式解析失败: {:?}", parsed.err());
        }
    }

    /// 没有进行中的登录时提交授权码，要明确报错（用户以为粘了就能成）。
    #[tokio::test]
    async fn submit_code_without_login_is_an_error() {
        let (h, _host) = handler();
        let out = h
            .handle(
                "SubmitChatGptLoginCode",
                serde_json::json!({ "input": "http://localhost:1455/auth/callback?code=x&state=y" }),
            )
            .await
            .expect("应被本域认领");
        assert!(out.is_err(), "没有登录在跑时提交应报错");
        assert!(out.unwrap_err().contains("没有进行中的登录"));
    }

    /// 没有进行中的登录时，取消要**明确报错**而不是静默成功——静默成功会
    /// 让面板显示"已取消"而端口仍被别的进程占着，用户点了没反应。
    #[tokio::test]
    async fn cancel_without_login_is_an_error() {
        let (h, _host) = handler();
        let out = h
            .handle("CancelChatGptLogin", serde_json::Value::Null)
            .await
            .expect("应被本域认领");
        assert!(out.is_err(), "没有登录在跑时取消应报错");
        assert!(out.unwrap_err().contains("没有进行中的"));
    }


}
