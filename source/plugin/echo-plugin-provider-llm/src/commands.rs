//! 供应商 / API 命令域：13 条命令由本插件处理（全插件化 S5）。
//!
//! ## 为什么这个命令域归本插件
//!
//! 池规则（`admin`）与 provider 工厂都在这里，**池的管理入口**也该在这里。
//! 此前 13 条命令的分支住在宿主 `agent/commands.rs`：宿主既知道"哪个 profile
//! 该激活"，又知道"切失败要回滚"，还知道"给面板的文案长什么样"——而它并不
//! 持有池的规则。搬过来之后：
//!
//! - 插件：命令名（注册关系）、载荷解析、事件形状、文案；
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
use echo_protocol::{BackendCommand, BackendEvent};

/// 本插件处理的命令名（`RenameApi` 是**进程级**命令，归组合根，不在此列）。
pub const COMMANDS: [&str; 13] = [
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

/// 宿主接缝：命令域需要而插件不该做的十三件事。
///
/// 每一项都是**状态迁移**（要 `AgentConfig` / provider 重建 / 落盘 / 事件
/// 出口 / 联邦查询），插件只负责"什么时候调、给什么参数、回什么文案"。
pub trait ProviderCommandHost: Send + Sync {
    /// 切模型（顶层 `[agent].model`）。
    fn switch_model<'a>(&'a self, model: &'a str) -> BoxFuture<'a, ()>;
    /// 切 provider（失败回滚到旧值）。
    fn switch_provider<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, ()>;
    /// 写系统提示词（宿主还要落盘到 `[plugins.system_prompt]` 并失效缓存）。
    fn set_system_prompt<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, ()>;
    /// 读系统提示词（纯读，事件由插件发）。
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
    /// 决定（`federation_rt` 的 `ApiProfiles` 分支），本插件只把它装进
    /// `RemoteApiProfiles` 事件——不在这里解析、也不重建类型，于是"对端加了
    /// 一个字段"不会让本插件丢掉它。
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
}

impl ProviderCommandHandler {
    /// 用宿主接缝构造处理器。
    pub fn new(host: Arc<dyn ProviderCommandHost>) -> Self {
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
                    "provider 命令载荷解析失败（{name}）: {error}"
                )))
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
            other => Err(format!(
                "provider 命令域不处理 {:?}（注册关系与命令泵不一致）",
                other.kind_name()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 记录调用顺序的假宿主：命令面搬到插件后，唯一可断言的就是"哪条命令
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
        fn set_session_api<'a>(&'a self, session_id: &'a str, profile: &'a str) -> BoxFuture<'a, ()> {
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
        ) -> BoxFuture<'a, Result<(Option<serde_json::Value>, Vec<serde_json::Value>), String>> {
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
            .handle("SendMessage", serde_json::json!({"session_id":"s","content":"x"}))
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
        h.handle(
            "RequestRemoteApiProfiles",
            serde_json::json!({"peer":"n1"}),
        )
        .await
        .unwrap()
        .unwrap();
        match &host.events()[0] {
            BackendEvent::RemoteApiProfiles { peer, error, profiles, .. } => {
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
        // 13 条命令都必须能解析并落到某个接缝（防"声明了但 match 漏了"）。
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
        });
        for name in COMMANDS {
            let payload = payloads[name].clone();
            let out = h.handle(name, payload).await;
            assert!(out.is_some(), "{name} 未被认领");
            assert!(out.unwrap().is_ok(), "{name} 解析失败");
        }
        assert_eq!(host.calls().len(), COMMANDS.len());
    }
}
