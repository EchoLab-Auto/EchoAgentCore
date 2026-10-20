//! 供应商 / API 命令域的**宿主能力实现**（全插件化 S5）。
//!
//! 与 `skills_host` / `tools_host` 同构：**命令路由与文案在插件**
//! （`echo_plugin_provider_llm::commands`），宿主只提供"只有它做得了"的
//! 那些事——状态迁移的原子动作：
//!
//! | 宿主提供 | 为什么只有它能做 |
//! |---|---|
//! | [`switch_model`](AgentProviderCommandHost) 等写操作 | 要 `Agent.model.config`（`Arc<RwLock<AgentConfig>>`）、重建 provider、写盘 |
//! | [`system_prompt`] | 提示词在 `Agent.model.config` |
//! | [`remote_api_profiles`] | 要联邦 querier（进程级端口集合） |
//! | [`test_api`] / [`query_api_balance`] / [`query_api_metrics`] | 要 HTTP 探针、计费端点、`MetricsStore` |
//!
//! 池的**规则**不在这里——它在插件的 `admin`（纯函数，输入 `PoolState`
//! 视图、输出 `PoolEffect`）。本文件是"把规则接到 Agent 上"的那层薄胶水。

use std::sync::Arc;

use echo_plugin_provider_llm::commands::{ApiUpdate, ProviderCommandHost};
use echo_protocol::BackendEvent;

use crate::agent::Agent;

/// 宿主适配器：一个 persona 视角的 API 管理面。
pub struct AgentProviderCommandHost {
    agent: Arc<Agent>,
}

impl AgentProviderCommandHost {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self { agent }
    }
}

impl ProviderCommandHost for AgentProviderCommandHost {
    fn switch_model<'a>(
        &'a self,
        model: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let agent = Arc::clone(&self.agent);
        let model = model.to_string();
        Box::pin(async move {
            agent.model.config.write().await.model = model.clone();
            agent.set_model(model.clone()).await;
            agent.emit_api_config().await;
            agent.emit(BackendEvent::Error {
                session_id: None,
                message: format!("switched model: {model}"),
            });
            let cfg = agent.model.config.read().await.clone();
            agent.persist_config(&cfg).await;
        })
    }

    fn switch_provider<'a>(
        &'a self,
        provider: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let agent = Arc::clone(&self.agent);
        let provider = provider.to_string();
        Box::pin(async move {
            let mut config = agent.model.config.write().await;
            let old_provider = config.provider.clone();
            config.provider = provider.clone();
            let cfg = config.clone();
            drop(config);
            if agent.rebuild_provider(&cfg).await {
                let model = cfg.model.clone();
                let base_url = cfg.effective_base_url();
                let key_set = !cfg.effective_api_key().is_empty();
                agent.set_model(model).await;
                agent.emit_api_config().await;
                agent.persist_config(&cfg).await;
                agent.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!(
                        "switched provider: {provider} ({base_url}, key: {})",
                        if key_set { "***" } else { "not set" }
                    ),
                });
            } else {
                // 构建失败 = 回滚（不留半生效状态）。
                let mut config = agent.model.config.write().await;
                config.provider = old_provider;
            }
        })
    }

    fn set_system_prompt<'a>(
        &'a self,
        prompt: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let agent = Arc::clone(&self.agent);
        let prompt = prompt.to_string();
        Box::pin(async move {
            agent.model.config.write().await.system_prompt = prompt.clone();
            agent.prompt.invalidate().await;
            agent.persist_system_prompt_plugin(&prompt).await;
            agent.emit(BackendEvent::SystemPrompt { text: prompt });
            agent.emit(BackendEvent::Error {
                session_id: None,
                message: "system prompt updated".into(),
            });
        })
    }

    fn system_prompt(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + '_>> {
        let agent = Arc::clone(&self.agent);
        Box::pin(async move { agent.model.config.read().await.system_prompt.clone() })
    }

    fn update_api<'a>(
        &'a self,
        update: ApiUpdate,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let agent = Arc::clone(&self.agent);
        Box::pin(async move {
            agent
                .update_api_config(
                    update.name,
                    update.provider,
                    update.model,
                    update.base_url,
                    update.api_key,
                    update.thinking,
                    update.reasoning_effort,
                )
                .await;
        })
    }

    fn switch_api<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let agent = Arc::clone(&self.agent);
        let name = name.to_string();
        Box::pin(async move { agent.switch_api(&name).await })
    }

    fn set_session_api<'a>(
        &'a self,
        session_id: &'a str,
        profile: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let agent = Arc::clone(&self.agent);
        let session_id = session_id.to_string();
        let profile = profile.to_string();
        Box::pin(async move {
            // 会话级 API 覆盖（2026-10）：该会话的 LLM 请求走指定 profile 的
            // provider；其余会话不受影响。team_id 已由 `requires_explicit_team`
            // 保证非空（会话键带归属，这里只需要会话 id）。
            //
            // 顺序：先解析（provider 构建成功）再落盘——失败时不写入，避免在
            // 工作区 JSON 里留下无法解析的覆盖（重启后会反复报 profile not
            // found）。2026-10 用户报障修正。
            let ok = agent.set_session_provider(&session_id, &profile).await;
            if ok {
                agent.persist_session_api_profile(&session_id, Some(profile.as_str()));
            }
            let message = if ok {
                if profile.is_empty() {
                    format!("会话 {session_id} 已恢复跟随 persona 级配置")
                } else {
                    format!("会话 {session_id} 已切换到 API: {profile}")
                }
            } else {
                format!("会话 {session_id} 切换 API 失败（profile 不存在或构建失败）")
            };
            agent.emit(BackendEvent::Error {
                session_id: Some(session_id.clone()),
                message,
            });
            agent.emit_api_config().await;
        })
    }

    fn clear_session_api<'a>(
        &'a self,
        session_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let agent = Arc::clone(&self.agent);
        let session_id = session_id.to_string();
        Box::pin(async move {
            // 清除会话级覆盖（回到跟随 persona 级），并同步清掉工作区对话里
            // 持久化的 `api_profile`。
            agent.persist_session_api_profile(&session_id, None);
            agent.set_session_provider(&session_id, "").await;
            agent.emit(BackendEvent::Error {
                session_id: Some(session_id.clone()),
                message: format!("会话 {session_id} 已恢复跟随 persona 级配置"),
            });
            agent.emit_api_config().await;
        })
    }

    fn remote_api_profiles<'a>(
        &'a self,
        peer: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        (Option<serde_json::Value>, Vec<serde_json::Value>),
                        String,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let agent = Arc::clone(&self.agent);
        let peer = peer.to_string();
        Box::pin(async move {
            // 分布式供应商共享（2026-10）：经联邦拉远端**脱敏**供应商池；
            // peer 离线 / 未实现该查询时 error 带原因。
            let result = match agent.federation().querier.clone() {
                Some(querier) => {
                    querier(
                        peer.clone(),
                        echo_federation::QueryKind::ApiProfiles,
                        String::new(),
                    )
                    .await
                }
                None => Err("联邦未接线：远程供应商查询不可用".into()),
            };
            match result {
                Ok(payload) => {
                    let active = payload.get("active").cloned();
                    let profiles = payload
                        .get("profiles")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    Ok((active, profiles))
                }
                Err(message) => Err(message),
            }
        })
    }

    fn test_api<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let agent = Arc::clone(&self.agent);
        let name = name.to_string();
        Box::pin(async move { agent.test_api_config(&name).await })
    }

    fn query_api_balance<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let agent = Arc::clone(&self.agent);
        let name = name.to_string();
        Box::pin(async move { agent.query_api_balance(&name).await })
    }

    fn query_api_metrics<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let agent = Arc::clone(&self.agent);
        let name = name.to_string();
        Box::pin(async move { agent.query_api_metrics(&name).await })
    }

    fn delete_api<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let agent = Arc::clone(&self.agent);
        let name = name.to_string();
        Box::pin(async move { agent.delete_api(&name).await })
    }

    fn emit(&self, event: BackendEvent) {
        self.agent.emit(event);
    }
}
