//! 模型出口服务（S4a）：provider / 配置 / 模型名 / profile 覆盖的**唯一持有者**。
//!
//! 这四件事必须**一起变**，这是本模块存在的理由：
//!
//! | 操作 | 要同时改的 | 漏一个的后果 |
//! |---|---|---|
//! | 切换 provider | `config.provider` + `provider` 实例 + `active_model` | 面板显示新 provider，实际仍打旧端点 |
//! | 切换 API profile | `config.active_api` + `persona_api` + `provider` 实例 | 重启后回退到默认，且会话级覆盖指向已删的 profile |
//! | 会话级覆盖 | `session_providers` + 该会话的 `session_provider_locks` | 并发构建重复 provider（连接泄漏） |
//!
//! 这些不变式此前散在 `impl Agent` 的 `api_admin.rs` 里——每个方法各改其中
//! 两三个字段，靠注释提醒"别忘了那个"。集中到一处之后，**新增一条切换路径时
//! 只有一个地方要看**。
//!
//! ## 为什么字段仍是 `pub(crate)` 而不是全封成方法
//!
//! S4a 的目标是**把字段搬出 `Agent`**、给后续阶段一个可传的句柄，不是立刻改变
//! 每个调用点的写法。纯机械的 `self.config` → `self.model.config` 是零语义变化
//! 的；而把 70 多个调用点逐一改成语义方法，会把"结构重构"和"接口重设计"混在
//! 一次提交里——出问题时分不清是哪一类改动引入的。
//!
//! 语义方法（[`ModelService::config_snapshot`] / [`ModelService::set_active_model`]
//! / [`ModelService::provider`] / [`ModelService::replace_provider`]）先覆盖**最常
//! 用**的几条；其余留给 S4c 逐步收敛。

use std::sync::Arc;

use dashmap::DashMap;

use crate::config::AgentConfig;
use crate::llm::LlmProvider;

/// 模型出口：一个 persona 的 provider 实例、配置快照与 per-session 覆盖。
///
/// 由 `Agent` 以 `Arc` 持有——`Arc` 而不是直接内联，是为了让后续阶段能把这个
/// 句柄**单独递给别的服务**（提示词装配不需要整只 `Agent`，只需要读配置），
/// 而不必绕过 `Agent` 再去查一遍。
pub struct ModelService {
    /// 当前生效的 provider 实例（已含计量 / 脱敏装饰，见组合根装配）。
    ///
    /// `tokio::sync::RwLock` 而不是 `std`：读发生在 async 路径上（`api_config`
    /// / `rebuild_provider` 等），用 std 锁会在跨 await 时阻塞 worker 线程。
    /// 这与改造前 `Agent` 上的类型一致——S4a 不改变任何同步语义。
    pub(crate) provider: tokio::sync::RwLock<Arc<dyn LlmProvider>>,
    /// 配置快照（运行期可变：SwitchModel / UpdateApiConfig / SwitchProvider…）。
    pub(crate) config: tokio::sync::RwLock<AgentConfig>,
    /// 当前模型名（provider 侧的 `default_model` 可能被配置覆盖）。
    pub(crate) active_model: tokio::sync::RwLock<String>,
    /// 配置文件句柄（持久化 `[agent]` 变更；None = 未接线，测试场景）。
    pub(crate) config_store: tokio::sync::Mutex<Option<echo_config::ConfigStore>>,
    /// Persona 级 API 供应商名（`None` = 跟随全局默认）。
    pub(crate) persona_api: tokio::sync::RwLock<Option<String>>,
    /// 会话级 provider 覆盖（`session_id → (profile_name, provider)`）。
    ///
    /// 为什么挂在模型服务而不是会话存储：provider 是**模型出口**级别的资源，
    /// 会话级覆盖只是它按会话的镜像；放进会话存储会让"会话"认识 LLM 类型。
    pub(crate) session_providers: DashMap<String, (String, Arc<dyn LlmProvider>)>,
    /// 会话级 provider 构建互斥（同会话串行、跨会话并行）。
    pub(crate) session_provider_locks: DashMap<String, tokio::sync::Mutex<()>>,
    /// 余额快照查询互斥（周期任务与手动「查余额」共用，防重复采样）。
    pub(crate) balance_snapshot_lock: tokio::sync::Mutex<()>,
}

impl ModelService {
    /// 用初始 provider + 配置构造（其余状态为空）。
    pub fn new(provider: Arc<dyn LlmProvider>, config: AgentConfig) -> Self {
        Self {
            provider: tokio::sync::RwLock::new(provider),
            config: tokio::sync::RwLock::new(config),
            active_model: tokio::sync::RwLock::new(String::new()),
            config_store: tokio::sync::Mutex::new(None),
            persona_api: tokio::sync::RwLock::new(None),
            session_providers: DashMap::new(),
            session_provider_locks: DashMap::new(),
            balance_snapshot_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// 当前 provider 实例（克隆 `Arc`，不持有锁）。
    ///
    /// 比 `self.model.provider.read().await.clone()` 少一层样板——而这层样板在
    /// 调用点出现 14 次。
    pub async fn provider(&self) -> Arc<dyn LlmProvider> {
        self.provider.read().await.clone()
    }

    /// 替换 provider 实例（配置由调用方一并维护，见模块文档的不变式表）。
    pub async fn replace_provider(&self, provider: Arc<dyn LlmProvider>) {
        *self.provider.write().await = provider;
    }

    /// 配置快照（克隆）。
    pub async fn config_snapshot(&self) -> AgentConfig {
        self.config.read().await.clone()
    }

    /// 当前模型名**原值**（空串 = 未设置；调用方应回退 provider 的
    /// `default_model()`——那个回退逻辑在 `Agent::active_model()` 里）。
    ///
    /// 不叫 `active_model()` 是刻意的：`Agent` 上有同名方法（**带回退**），
    /// 名字相同时机械改写会把"带回退"静默换成"不带回退"——编译器发现不了，
    /// 只有测试能发现（本次实测就撞了一次）。
    pub async fn model_name(&self) -> String {
        self.active_model.read().await.clone()
    }

    /// 设置当前模型名。
    pub async fn set_active_model(&self, model: impl Into<String>) {
        *self.active_model.write().await = model.into();
    }

    /// 某会话的 provider 覆盖（`None` = 该会话跟随 persona 默认）。
    pub fn session_provider(&self, session_id: &str) -> Option<Arc<dyn LlmProvider>> {
        self.session_providers
            .get(session_id)
            .map(|entry| Arc::clone(&entry.value().1))
    }

    /// 清掉某会话的 provider 覆盖（切回 persona 默认时用）。
    pub fn clear_session_provider(&self, session_id: &str) {
        self.session_providers.remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmProvider;
    use async_trait::async_trait;

    struct Noop;
    #[async_trait]
    impl LlmProvider for Noop {
        fn name(&self) -> &str {
            "noop"
        }
        fn default_model(&self) -> &str {
            "m"
        }
        async fn chat(
            &self,
            _request: &crate::llm::ChatRequest,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Api("unused".into()))
        }
        async fn chat_stream(
            &self,
            _request: &crate::llm::ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<crate::llm::ChatChunk>,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Api("unused".into()))
        }
    }

    /// 初始状态：provider 就位、模型名为空、无会话覆盖。
    #[tokio::test]
    async fn new_starts_with_empty_session_state() {
        let svc = ModelService::new(Arc::new(Noop), AgentConfig::default());
        assert_eq!(svc.provider().await.name(), "noop");
        assert_eq!(
            svc.model_name().await,
            "",
            "空串 = 未设置，调用方回退 config.model"
        );
        assert!(svc.session_provider("s1").is_none());
    }

    /// 模型名往返 + provider 替换（两条最常用的语义方法）。
    #[tokio::test]
    async fn model_name_and_provider_roundtrip() {
        let svc = ModelService::new(Arc::new(Noop), AgentConfig::default());
        svc.set_active_model("kimi-k3").await;
        assert_eq!(svc.model_name().await, "kimi-k3");
        svc.replace_provider(Arc::new(Noop)).await;
        assert_eq!(svc.provider().await.name(), "noop");
        // 配置快照是克隆：改快照不影响服务
        let mut snap = svc.config_snapshot().await;
        snap.model = "x".into();
        assert_ne!(svc.config_snapshot().await.model, "x");
    }

    /// 会话级覆盖：设置后能按会话取到，清掉后回到 persona 默认（None）。
    #[test]
    fn session_provider_set_and_clear() {
        let svc = ModelService::new(Arc::new(Noop), AgentConfig::default());
        svc.session_providers
            .insert("s1".into(), ("prof".into(), Arc::new(Noop)));
        assert!(svc.session_provider("s1").is_some());
        assert!(svc.session_provider("s2").is_none());
        svc.clear_session_provider("s1");
        assert!(svc.session_provider("s1").is_none(), "清掉后应回落到默认");
    }
}
