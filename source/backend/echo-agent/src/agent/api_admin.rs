//! API 供应商与 profile 管理（API 设置面）。
//!
//! 从 `agent/mod.rs` 拆出（框架优化）：`Agent` 的 API 配置 CRUD（update/
//! switch/delete/test/balance）、provider 热重建、配置持久化与探测配置
//! 解析。边界干净：只依赖 config + provider 工厂 + emit。纯移动零行为变化。

use super::*;

impl Agent {
    /// Current API configuration summary (for the TUI settings form).
    pub async fn api_config(&self) -> AgentConfig {
        self.config.read().await.clone()
    }

    /// Emit the current API configuration to subscribers.
    pub async fn emit_api_config(&self) {
        let cfg = self.api_config().await;
        let mut resolved = cfg.clone();
        resolved.apply_active_profile();
        let key_set = !resolved.effective_api_key().is_empty();
        let profiles: Vec<crate::event::ApiProfileInfo> = cfg
            .api_profiles
            .iter()
            .map(|p| crate::event::ApiProfileInfo {
                name: p.name.clone(),
                provider: p.provider.clone(),
                model: p.model.clone(),
                base_url: p.base_url.clone(),
                api_key_set: !p.api_key.is_empty(),
                thinking: p.thinking,
                reasoning_effort: p.reasoning_effort,
            })
            .collect();
        self.emit(BackendEvent::ApiConfigUpdated {
            provider: resolved.provider.clone(),
            model: self.active_model().await,
            base_url: resolved.effective_base_url(),
            api_key_set: key_set,
            thinking: resolved.thinking,
            reasoning_effort: resolved.reasoning_effort,
            system_prompt: cfg.system_prompt.clone(),
            active_api: cfg.active_api.clone(),
            profiles: profiles.clone(),
        });
        self.emit(BackendEvent::ApiProfilesUpdated {
            active_api: cfg.active_api.clone(),
            profiles,
        });
    }

    /// Rebuild the LLM provider from the current config snapshot.
    ///
    /// 重建时统一包两层装饰器：计量（`llm::wrap_metering`——token 用量记录，
    /// 覆盖全部 LLM 出口，供余额/用量图表）+ 脱敏（`llm::wrap_redacting`——
    /// LLM 请求出口卡口）；同时把快照中的密钥登记进脱敏器——热更新后新密钥
    /// 立即纳入全部出口的拦截范围（幂等）。
    pub(crate) async fn rebuild_provider(&self, cfg: &AgentConfig) -> bool {
        let mut resolved = cfg.clone();
        resolved.apply_active_profile();
        resolved.api_key = resolved.effective_api_key();
        resolved.base_url = resolved.effective_base_url();
        self.register_config_secrets(&resolved);
        match crate::llm::create_provider(&resolved) {
            Ok(p) => {
                let provider: Arc<dyn LlmProvider> = Arc::from(p);
                let provider = match self.metrics() {
                    Some(store) => {
                        crate::llm::wrap_metering(provider, store, cfg.active_api.clone())
                    }
                    None => provider,
                };
                let provider = match self.redactor() {
                    Some(redactor) => crate::llm::wrap_redacting(provider, redactor),
                    None => provider,
                };
                *self.provider.write().await = provider;
                true
            }
            Err(e) => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("failed to build provider: {e}"),
                });
                false
            }
        }
    }

    /// 把配置快照中的全部密钥登记进脱敏器（幂等；未注入脱敏器 = no-op）。
    ///
    /// 覆盖顶层 `api_key`（已含 env 回退后的生效值）与 `api_profiles[*]`。
    /// 旧密钥保留登记——它同样敏感（例如切换 profile 后被替换的旧 key）。
    pub(crate) fn register_config_secrets(&self, cfg: &AgentConfig) {
        let Some(redactor) = self.redactor() else {
            return;
        };
        if !cfg.api_key.trim().is_empty() {
            redactor.register_secret(&cfg.api_key, "api_key");
        }
        for profile in &cfg.api_profiles {
            if !profile.api_key.trim().is_empty() {
                redactor.register_secret(&profile.api_key, &format!("api_key:{}", profile.name));
            }
        }
    }

    /// 供应商池快照：共享 ConfigStore（core.toml，管理面每次变更都会写回）
    /// 优先，读不到时回退本 agent 的配置快照（单元测试 / 未接线场景）。
    ///
    /// 为什么读共享文件而不是 `self.config`：配置变更命令（UpdateApiConfig /
    /// DeleteApi）在**管理面** agent 上执行，各 persona 持有的配置克隆可能
    /// 落后——而池是全局的（见 [`crate::api_pool`] 模块文档）。
    pub(crate) async fn api_pool_snapshot(&self) -> Vec<ApiProfile> {
        if let Some(store) = self.config_store.lock().await.clone() {
            if let Ok(snapshot) = crate::api_pool::read_snapshot(&store) {
                return snapshot.profiles;
            }
        }
        self.config.read().await.api_profiles.clone()
    }

    /// 子代理单次委派的 provider（`spawn_subagent` 的 `profile` 参数）：
    /// 按名从供应商池解析 → 构建 → 计量 + 脱敏装饰（与主 provider 同标准）。
    ///
    /// fail-closed：profile 不存在 / 缺 key / 构建失败都返回 Err（附可用
    /// 清单），由调用方经 `<subagent_event>` 回报——**不静默回退主模型**，
    /// 避免"指定了便宜模型实际烧了贵模型"的意外。
    pub(crate) async fn subagent_provider_for(
        &self,
        name: &str,
    ) -> Result<Arc<dyn LlmProvider>, String> {
        let mut config = self.config.read().await.clone();
        config.api_profiles = self.api_pool_snapshot().await;
        if !config.api_profiles.iter().any(|p| p.name == name) {
            let available: Vec<&str> = config
                .api_profiles
                .iter()
                .map(|p| p.name.as_str())
                .collect();
            return Err(if available.is_empty() {
                format!(
                    "模型 profile「{name}」不存在（当前未配置任何供应商 profile，可在 Panel「API 设置」添加）"
                )
            } else {
                format!(
                    "模型 profile「{name}」不存在（可用: {}）",
                    available.join(", ")
                )
            });
        }
        let probe = resolve_probe_config(&config, name)?;
        if probe.api_key.trim().is_empty() {
            return Err(format!(
                "模型 profile「{name}」未配置 API Key（可在 Panel「API 设置」补填，或设置对应环境变量）"
            ));
        }
        // 安全：新 provider 登记该 profile 的密钥（幂等）；计量归属 = profile
        // 名（余额/用量图表的命名空间一致）。
        self.register_config_secrets(&probe);
        let built = crate::llm::create_provider(&probe)
            .map_err(|e| format!("模型 profile「{name}」provider 构建失败: {e}"))?;
        let provider: Arc<dyn LlmProvider> = Arc::from(built);
        let provider = match self.metrics() {
            Some(store) => crate::llm::wrap_metering(provider, store, name.to_string()),
            None => provider,
        };
        let provider = match self.redactor() {
            Some(redactor) => crate::llm::wrap_redacting(provider, redactor),
            None => provider,
        };
        Ok(provider)
    }

    /// Save an API profile (or the top-level default when `name` is empty),
    /// activate it, rebuild the provider, and persist.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn update_api_config(
        &self,
        name: String,
        provider: String,
        model: String,
        base_url: String,
        api_key: String,
        thinking: Option<crate::config::ThinkingMode>,
        reasoning_effort: Option<crate::config::ReasoningEffort>,
    ) {
        let mut config = self.config.write().await;
        let keep_key = api_key.is_empty();
        // 新建命名 profile 时，空的 provider/model/base_url 从当前生效配置
        // 继承，避免写出残缺 profile：残缺 profile 单独做 TestApi 时必然
        // "provider build failed"，且极易误导用户以为 LLM 整体不可用。
        // （更新已有 profile 时保持"空 = 保留该 profile 原值"的语义。）
        let (provider, model, base_url) =
            if !name.is_empty() && !config.api_profiles.iter().any(|p| p.name == name) {
                let mut resolved = config.clone();
                resolved.apply_active_profile();
                (
                    if provider.is_empty() {
                        resolved.provider
                    } else {
                        provider
                    },
                    if model.is_empty() {
                        resolved.model
                    } else {
                        model
                    },
                    if base_url.is_empty() {
                        resolved.base_url
                    } else {
                        base_url
                    },
                )
            } else {
                (provider, model, base_url)
            };
        if name.is_empty() {
            if !provider.is_empty() {
                config.provider = provider.clone();
            }
            if !model.is_empty() {
                config.model = model.clone();
            }
            if !base_url.is_empty() {
                config.base_url = base_url.clone();
            }
            if !keep_key {
                config.api_key = api_key.clone();
            }
            if let Some(thinking) = thinking {
                config.thinking = thinking;
            }
            if let Some(reasoning_effort) = reasoning_effort {
                config.reasoning_effort = reasoning_effort;
            }
            // Don't clear active_api — let explicit SwitchApi handle that.
        } else {
            let profile = match config.api_profiles.iter_mut().find(|p| p.name == name) {
                Some(p) => p,
                None => {
                    config.api_profiles.push(ApiProfile::new(
                        name.clone(),
                        provider.clone(),
                        model.clone(),
                    ));
                    let idx = config.api_profiles.len() - 1;
                    &mut config.api_profiles[idx]
                }
            };
            if !provider.is_empty() {
                profile.provider = provider.clone();
            }
            if !model.is_empty() {
                profile.model = model.clone();
            }
            if !base_url.is_empty() {
                profile.base_url = base_url.clone();
            }
            if !keep_key {
                profile.api_key = api_key.clone();
            }
            if let Some(thinking) = thinking {
                profile.thinking = thinking;
            }
            if let Some(reasoning_effort) = reasoning_effort {
                profile.reasoning_effort = reasoning_effort;
            }
            config.active_api = name.clone();
        }
        let cfg_snapshot = config.clone();
        drop(config);

        self.set_model(model.clone()).await;
        if self.rebuild_provider(&cfg_snapshot).await {
            self.emit(BackendEvent::Error {
                session_id: None,
                message: if name.is_empty() {
                    format!("default API updated: {provider} / {model}")
                } else {
                    format!("API saved and activated: {name} ({provider} / {model})")
                },
            });
        }
        self.emit_api_config().await;
        self.persist_config(&cfg_snapshot).await;
    }

    /// Switch to an API profile (or the top-level default when name is empty).
    pub(crate) async fn switch_api(&self, name: &str) {
        let mut config = self.config.write().await;
        if name.is_empty() {
            config.active_api.clear();
        } else {
            if !config.api_profiles.iter().any(|p| p.name == name) {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("API config not found: {name}"),
                });
                return;
            }
            config.active_api = name.to_string();
        }
        let cfg_snapshot = config.clone();
        let mut resolved = cfg_snapshot.clone();
        resolved.apply_active_profile();
        let model = resolved.model.clone();
        drop(config);

        if self.rebuild_provider(&cfg_snapshot).await {
            self.set_model(model).await;
            let label = if name.is_empty() {
                "default config".to_string()
            } else {
                name.to_string()
            };
            self.emit(BackendEvent::Error {
                session_id: None,
                message: format!("switched to API: {label}"),
            });
        }
        self.emit_api_config().await;
        self.persist_config(&cfg_snapshot).await;
    }

    /// Delete an API profile; if it was active, fall back to the top-level default.
    pub(crate) async fn delete_api(&self, name: &str) {
        let mut config = self.config.write().await;
        let before = config.api_profiles.len();
        config.api_profiles.retain(|p| p.name != name);
        let removed = config.api_profiles.len() < before;
        if removed && config.active_api == name {
            config.active_api.clear();
        }
        let cfg_snapshot = config.clone();
        drop(config);

        if removed {
            if self.rebuild_provider(&cfg_snapshot).await {
                self.set_model(cfg_snapshot.model.clone()).await;
            }
            self.emit(BackendEvent::Error {
                session_id: None,
                message: format!("deleted API config: {name}"),
            });
            self.emit_api_config().await;
            self.persist_config(&cfg_snapshot).await;
        } else {
            self.emit(BackendEvent::Error {
                session_id: None,
                message: format!("API config not found: {name}"),
            });
        }
    }

    /// Test connectivity of an API config by sending a minimal probe request.
    ///
    /// `name` empty tests the active (top-level) config; otherwise the named
    /// profile's values are used. Emits `BackendEvent::ApiTestResult`; the
    /// active provider is never modified.
    pub(crate) async fn test_api_config(&self, name: &str) {
        let timestamp_start = std::time::Instant::now();
        let config = self.config.read().await.clone();

        // Resolve the config to test: named profile or the active default.
        let probe = match resolve_probe_config(&config, name) {
            Ok(probe) => probe,
            Err(message) => {
                self.emit(BackendEvent::ApiTestResult {
                    name: name.into(),
                    ok: false,
                    message,
                    latency_ms: timestamp_start.elapsed().as_millis() as u64,
                });
                return;
            }
        };

        // Build a throwaway provider from the probe config.
        let provider = match create_provider(&probe) {
            Ok(provider) => provider,
            Err(error) => {
                self.emit(BackendEvent::ApiTestResult {
                    name: name.into(),
                    ok: false,
                    message: format!("provider build failed: {error}"),
                    latency_ms: timestamp_start.elapsed().as_millis() as u64,
                });
                return;
            }
        };

        let model = probe.model.clone();
        let request = ChatRequest {
            model: model.clone(),
            messages: vec![ChatMessage::user("ping")],
            tools: None,
            temperature: Some(0.0),
            max_tokens: Some(4),
        };
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(20), provider.chat(&request)).await;

        let latency_ms = timestamp_start.elapsed().as_millis() as u64;
        let (ok, message) = match result {
            Ok(Ok(response)) => (
                true,
                match response.content {
                    Some(content) if !content.trim().is_empty() => format!(
                        "OK ({}) — 响应: {}",
                        model,
                        echo_defs::token::truncate(content.trim(), 60)
                    ),
                    _ => format!("OK ({model}) — 收到空响应"),
                },
            ),
            Ok(Err(error)) => (false, format!("请求失败: {error}")),
            Err(_) => (false, "请求超时（>20s）".into()),
        };
        self.emit(BackendEvent::ApiTestResult {
            name: name.into(),
            ok,
            message,
            latency_ms,
        });
    }

    /// 查询 API 账户余额（目前仅 DeepSeek 官方端点支持 `/user/balance`）。
    ///
    /// `name` 空 = 全局默认配置，非空 = 该 profile；用 profile 自身的
    /// api_key / base_url 请求。成功时同时落一条本地余额快照（图表数据源）。
    pub(crate) async fn query_api_balance(&self, name: &str) {
        let config = self.config.read().await.clone();
        let probe = match resolve_probe_config(&config, name) {
            Ok(probe) => probe,
            Err(message) => {
                self.emit_balance_fail(name, message).await;
                return;
            }
        };
        match fetch_balance(&probe).await {
            Ok(fetched) => {
                // 成功即落快照：手动查询与周期任务共享同一数据源。
                if let Some(store) = self.metrics() {
                    store.record_balance(
                        name,
                        &fetched.currency,
                        parse_amount(&fetched.total),
                        parse_amount(&fetched.granted),
                        parse_amount(&fetched.topped_up),
                    );
                }
                self.emit(BackendEvent::ApiBalanceResult {
                    name: name.into(),
                    ok: true,
                    available: fetched.available,
                    total: fetched.total,
                    granted: fetched.granted,
                    topped_up: fetched.topped_up,
                    currency: fetched.currency,
                    message: if fetched.available {
                        "余额已更新".into()
                    } else {
                        "账户当前不可用（余额不足或已停用）".into()
                    },
                });
            }
            Err(message) => self.emit_balance_fail(name, message).await,
        }
    }

    /// 余额查询失败时发 `ApiBalanceResult{ok:false}`。
    pub(crate) async fn emit_balance_fail(&self, name: &str, message: String) {
        self.emit(BackendEvent::ApiBalanceResult {
            name: name.into(),
            ok: false,
            available: false,
            total: String::new(),
            granted: String::new(),
            topped_up: String::new(),
            currency: String::new(),
            message,
        });
    }

    /// 响应 `QueryApiMetrics`：返回本地积累的余额/用量序列（近 7 天窗口）。
    ///
    /// `name` 空 = 默认配置 + 全部 profile；非空 = 指定 profile。每个目标
    /// 一条 entry（暂无数据 = 空序列）。费用估算按 `[agent.pricing]` 定价表。
    pub(crate) async fn query_api_metrics(&self, name: &str) {
        let Some(store) = self.metrics() else {
            // 未接线（测试 / 旧装配）：回空表，面板按"数据积累中"处理。
            self.emit(BackendEvent::ApiMetrics {
                entries: Vec::new(),
            });
            return;
        };
        let config = self.config.read().await.clone();
        let names: Vec<String> = if name.is_empty() {
            metrics_targets(&config)
        } else {
            vec![name.to_string()]
        };
        let since = crate::metrics::now_ms() - crate::metrics::METRICS_WINDOW_MS;
        let mut entries = Vec::with_capacity(names.len());
        for target in names {
            let balance = store
                .balance_points(&target, since)
                .into_iter()
                .map(|point| crate::event::ApiBalancePoint {
                    ts_ms: point.ts_ms,
                    total: point.total,
                    granted: point.granted,
                    topped_up: point.topped_up,
                    currency: point.currency,
                })
                .collect();
            let mut cost_currency = String::new();
            let usage = store
                .usage_slices(&target, since)
                .into_iter()
                .map(|slice| {
                    let cost = crate::metrics::estimate_cost(
                        &config.pricing,
                        &slice.model,
                        slice.ts_ms,
                        slice.prompt_tokens,
                        slice.completion_tokens,
                    );
                    if cost_currency.is_empty() {
                        if let Some((_, currency)) = &cost {
                            cost_currency = currency.clone();
                        }
                    }
                    crate::event::ApiUsagePoint {
                        ts_ms: slice.ts_ms,
                        model: slice.model,
                        prompt_tokens: slice.prompt_tokens,
                        completion_tokens: slice.completion_tokens,
                        calls: slice.calls,
                        est_cost: cost.map(|(value, _)| value),
                    }
                })
                .collect();
            entries.push(crate::event::ApiMetricsEntry {
                name: target,
                balance,
                usage,
                cost_currency,
            });
        }
        self.emit(BackendEvent::ApiMetrics { entries });
    }

    /// 启动周期余额快照任务（组合根对全局管理 Agent 调用一次）。
    ///
    /// 每 `[agent].balance_snapshot_secs` 秒（默认 600；0 = 关闭；下限 30s）
    /// 对所有可查询目标（顶层默认 + 各 profile，要求 DeepSeek 端点且有 key）
    /// 调一次 `/user/balance` 落快照。失败静默（debug 日志）——面向用户的
    /// 报错由手动「查余额」路径呈现。
    pub fn start_balance_snapshot_task(self: &Arc<Self>) {
        let agent = Arc::clone(self);
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let period_secs = agent.config.read().await.balance_snapshot_secs;
            if period_secs == 0 {
                tracing::info!("balance snapshot task disabled (balance_snapshot_secs = 0)");
                return;
            }
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(period_secs.max(30)));
            loop {
                tokio::select! {
                    _ = interval.tick() => agent.snapshot_balances_once().await,
                    _ = cancel.cancelled() => break,
                }
            }
        });
    }

    /// 单次快照：给所有可查询目标记录一条余额快照。
    async fn snapshot_balances_once(&self) {
        let Some(store) = self.metrics() else {
            return;
        };
        let config = self.config.read().await.clone();
        let targets = metrics_targets(&config);
        for target in targets {
            let probe = match resolve_probe_config(&config, &target) {
                Ok(probe) => probe,
                Err(_) => continue,
            };
            if deepseek_balance_endpoint(&probe.base_url).is_none()
                || probe.effective_api_key().is_empty()
            {
                continue;
            }
            match fetch_balance(&probe).await {
                Ok(fetched) => store.record_balance(
                    &target,
                    &fetched.currency,
                    parse_amount(&fetched.total),
                    parse_amount(&fetched.granted),
                    parse_amount(&fetched.topped_up),
                ),
                Err(error) => {
                    tracing::debug!(target = %target, %error, "balance snapshot failed");
                }
            }
        }
    }

    /// Persist the system prompt plugin text to `[plugins.system_prompt]`
    /// instead of `[agent]`. The API config no longer owns the system prompt.
    pub(crate) async fn persist_system_prompt_plugin(&self, text: &str) {
        let store = match self.config_store.lock().await.clone() {
            Some(s) => s,
            None => return,
        };
        let text = text.to_string();
        if let Err(error) = store.patch(|root| {
            let plugins = echo_adapter::ensure_table(root, "plugins");
            let system_prompt = echo_adapter::ensure_table(plugins, "system_prompt");
            system_prompt.insert("text".into(), toml::Value::String(text));
            Ok(())
        }) {
            tracing::warn!(error = %error, "failed to persist system prompt plugin");
        }
    }

    /// Persist a config snapshot's `[agent]` section back to the TOML file
    /// through the shared [`echo_adapter::ConfigStore`].
    pub(crate) async fn persist_config(&self, cfg: &AgentConfig) {
        let store = match self.config_store.lock().await.clone() {
            Some(s) => s,
            None => return,
        };
        let mut value = match toml::Value::try_from(cfg) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize agent config");
                return;
            }
        };
        tracing::debug!(api_key_len = cfg.api_key.len(), "persisting agent config");
        if let Err(e) = store.patch(|root| {
            // `api_key` is excluded from AgentConfig's generic serialization.
            // Write the explicit in-memory key when set; otherwise keep the
            // on-disk value so unrelated persists never strip it from the file.
            let key_value = if cfg.api_key.is_empty() {
                root.get("agent")
                    .and_then(|agent| agent.get("api_key"))
                    .cloned()
            } else {
                Some(toml::Value::String(cfg.api_key.clone()))
            };
            if let Some(key_value) = key_value {
                value
                    .as_table_mut()
                    .expect("serialized agent config is a table")
                    .insert("api_key".into(), key_value);
            }
            // `teams` / `disabled_teams` 由 AgentManager 独立持久化
            // （config_writer 只写这两张表）。人格快照里的副本可能已过期，
            // 这里一律以磁盘为准，避免"某次无关保存把新人格名单写回旧值"。
            let table = value
                .as_table_mut()
                .expect("serialized agent config is a table");
            for key in ["teams", "disabled_teams"] {
                let disk = root.get("agent").and_then(|agent| agent.get(key)).cloned();
                match disk {
                    Some(v) => {
                        table.insert(key.into(), v);
                    }
                    None => {
                        table.remove(key);
                    }
                }
            }
            root.insert("agent".into(), value.clone());
            Ok(())
        }) {
            tracing::warn!(error = %e, "failed to persist agent config");
            return;
        }
        tracing::info!(path = %store.path().display(), "agent config persisted");
    }
}

/// 一次余额查询的结果（金额为原始字符串，如 "1287.91"）。
pub(crate) struct BalanceFetch {
    pub available: bool,
    pub total: String,
    pub granted: String,
    pub topped_up: String,
    pub currency: String,
}

/// 请求 DeepSeek `/user/balance`（15s 超时；无事件 / 记录副作用——供手动
/// 查询与周期快照共用）。
pub(crate) async fn fetch_balance(probe: &AgentConfig) -> Result<BalanceFetch, String> {
    #[derive(serde::Deserialize)]
    struct BalanceResponse {
        #[serde(default)]
        is_available: bool,
        #[serde(default)]
        balance_infos: Vec<BalanceInfo>,
    }
    #[derive(serde::Deserialize)]
    struct BalanceInfo {
        #[serde(default)]
        currency: String,
        #[serde(default)]
        total_balance: String,
        #[serde(default)]
        granted_balance: String,
        #[serde(default)]
        topped_up_balance: String,
    }

    let Some(endpoint) = deepseek_balance_endpoint(&probe.base_url) else {
        return Err(format!(
            "余额查询仅支持 DeepSeek 官方端点（当前 base_url: {}）",
            probe.base_url
        ));
    };
    let api_key = probe.effective_api_key();
    if api_key.is_empty() {
        return Err("缺少 API Key，无法查询余额".into());
    }

    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("HTTP client build failed: {e}"))?;
        let response = client
            .get(&endpoint)
            .bearer_auth(&api_key)
            .send()
            .await
            .map_err(|e| format!("请求失败: {e}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("读取响应失败: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "HTTP {status}: {}",
                echo_defs::token::truncate(&text, 200)
            ));
        }
        let parsed: BalanceResponse = serde_json::from_str(&text).map_err(|e| {
            format!(
                "响应解析失败: {e} — body: {}",
                echo_defs::token::truncate(&text, 200)
            )
        })?;
        Ok::<BalanceResponse, String>(parsed)
    })
    .await;

    match result {
        Ok(Ok(parsed)) => {
            let info = parsed.balance_infos.first();
            Ok(BalanceFetch {
                available: parsed.is_available,
                total: info.map(|i| i.total_balance.clone()).unwrap_or_default(),
                granted: info.map(|i| i.granted_balance.clone()).unwrap_or_default(),
                topped_up: info
                    .map(|i| i.topped_up_balance.clone())
                    .unwrap_or_default(),
                currency: info.map(|i| i.currency.clone()).unwrap_or_default(),
            })
        }
        Ok(Err(message)) => Err(message),
        Err(_) => Err("请求超时（>15s）".into()),
    }
}

/// 宽松解析金额字符串（失败 = 0.0；不阻断快照记录）。
pub(crate) fn parse_amount(text: &str) -> f64 {
    text.trim().parse::<f64>().unwrap_or(0.0)
}

/// 「全部目标」列表：顶层默认（`""`）+ 全部 profile 名。
///
/// 顶层默认与 active profile 指向同一配置时（`active_api` 命中池中某
/// profile——空名的解析会合入该 profile），去重跳过空名，避免同一账户
/// 产生双份序列。
fn metrics_targets(config: &AgentConfig) -> Vec<String> {
    let active = config.active_api.trim();
    let mut targets = Vec::with_capacity(config.api_profiles.len() + 1);
    if active.is_empty() || !config.api_profiles.iter().any(|p| p.name == active) {
        targets.push(String::new());
    }
    targets.extend(config.api_profiles.iter().map(|p| p.name.clone()));
    targets
}

/// Build the config snapshot used for a connectivity probe.
///
/// `name` empty → active (top-level) config with the active profile merged
/// in; otherwise the named profile's non-empty values override the current
/// effective config. Returns an error string when the profile does not exist
/// or the resolved config has no provider.
/// 从 base_url 推导 DeepSeek 余额查询端点。
///
/// `https://api.deepseek.com/anthropic` / `.../v1` / `.../beta` / 裸域
/// 统一映射为 `{root}/user/balance`；非 DeepSeek 域返回 None。
pub(crate) fn deepseek_balance_endpoint(base_url: &str) -> Option<String> {
    let mut root = base_url.trim().trim_end_matches('/');
    for suffix in ["/anthropic", "/v1", "/beta"] {
        if let Some(stripped) = root.strip_suffix(suffix) {
            root = stripped;
            break;
        }
    }
    if root.contains("deepseek.com") {
        Some(format!("{root}/user/balance"))
    } else {
        None
    }
}

pub(crate) fn resolve_probe_config(
    config: &crate::config::AgentConfig,
    name: &str,
) -> Result<crate::config::AgentConfig, String> {
    let mut probe = config.clone();
    if name.is_empty() {
        probe.apply_active_profile();
    } else {
        let profile = config
            .api_profiles
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("profile not found: {name}"))?;
        // 与 apply_active_profile 一致的宽松语义：profile 的空字段
        // 回退到当前生效值，避免字段残缺的 profile 测不出本来的配置。
        if !profile.provider.is_empty() {
            probe.provider = profile.provider.clone();
        }
        if !profile.model.is_empty() {
            probe.model = profile.model.clone();
        }
        if !profile.base_url.is_empty() {
            probe.base_url = profile.base_url.clone();
        }
        if !profile.api_key.is_empty() {
            probe.api_key = profile.api_key.clone();
        }
        probe.thinking = profile.thinking;
        probe.reasoning_effort = profile.reasoning_effort;
    }
    probe.api_key = probe.effective_api_key();
    probe.base_url = probe.effective_base_url();
    if probe.provider.is_empty() {
        return Err(format!(
            "provider not set (name={name}); 请先在设置中配置 Provider/API 或激活某个 profile"
        ));
    }
    Ok(probe)
}
