//! API 供应商与 profile 管理（API 设置面）。
//!
//! 从 `agent/mod.rs` 拆出（框架优化）：`Agent` 的 API 配置 CRUD（update/
//! switch/delete/test/balance）、provider 热重建、配置持久化与探测配置
//! 解析。边界干净：只依赖 config + provider 工厂 + emit。纯移动零行为变化。

use super::*;

impl Agent {
    /// Current API configuration summary (for the TUI settings form).
    pub async fn api_config(&self) -> AgentConfig {
        self.model.config.read().await.clone()
    }

    /// Emit the current API configuration to subscribers.
    ///
    /// 供应商池与全局激活项以共享 ConfigStore（core.toml）为准：发事件的
    /// 可能是任意 persona（如 SetSessionApi 之后的回推），其内存快照可能
    /// 落后于管理面——否则管理面新加的 profile 会被"看回去"，面板下拉
    /// 瞬间少项（2026-10 用户报障的连带效应）。
    pub async fn emit_api_config(&self) {
        for event in self.api_config_events().await {
            self.emit(event);
        }
    }

    /// 供应商配置快照的**事件形态**（不发，只组装）。
    ///
    /// 拆出来是为了让"面板重连的全量快照"（`RequestState`）能把这几条事件
    /// 和其它快照事件**按序装进一份列表**再一次性发出——而不是在插件里
    /// 先调一个 emit 再调另一个（顺序就成了调用顺序的副作用）。
    pub async fn api_config_events(&self) -> Vec<BackendEvent> {
        let mut cfg = self.api_config().await;
        if let Some(snapshot) = self.api_pool_store_snapshot().await {
            cfg.api_profiles = snapshot.profiles;
            cfg.active_api = snapshot.active_api;
        }
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
        let model = self.active_model().await;
        vec![
            BackendEvent::ApiConfigUpdated {
                provider: resolved.provider.clone(),
                model,
                base_url: resolved.effective_base_url(),
                api_key_set: key_set,
                thinking: resolved.thinking,
                reasoning_effort: resolved.reasoning_effort,
                system_prompt: cfg.system_prompt.clone(),
                active_api: cfg.active_api.clone(),
                profiles: profiles.clone(),
            },
            BackendEvent::ApiProfilesUpdated {
                active_api: cfg.active_api.clone(),
                profiles,
            },
        ]
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
                *self.model.provider.write().await = provider;
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
    /// 为什么读共享文件而不是 `self.model.config`：配置变更命令（UpdateApiConfig /
    /// DeleteApi）在**管理面** agent 上执行，各 persona 持有的配置克隆可能
    /// 落后——而池是全局的（见 [`crate::api_pool`] 模块文档）。
    pub(crate) async fn api_pool_snapshot(&self) -> Vec<ApiProfile> {
        if let Some(snapshot) = self.api_pool_store_snapshot().await {
            return snapshot.profiles;
        }
        self.model.config.read().await.api_profiles.clone()
    }

    /// 共享存储的**完整**池快照（profiles + 全局激活项 + 顶层默认）。
    ///
    /// `None` = 未接线 / 读取失败；调用方自行回退本快照（event 发射等场景
    /// 需要 active_api 一并取共享存储口径，避免 persona 过期快照把面板里
    /// 新加的 profile「看回去」）。
    pub(crate) async fn api_pool_store_snapshot(&self) -> Option<crate::api_pool::PoolSnapshot> {
        let store = self.model.config_store.lock().await.clone()?;
        crate::api_pool::read_snapshot(&store).ok()
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
        let mut config = self.model.config.read().await.clone();
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
        use echo_plugin_provider_llm::admin as pool;
        let mut config = self.model.config.write().await;
        // 继承基准：**当前生效**配置（`active_api` 解析后的那份）。新建
        // profile 时空字段从这里取，避免写出残缺 profile——残缺 profile
        // 单独 TestApi 必然 "provider build failed"，且极易误导用户以为
        // LLM 整体不可用。
        let defaults = {
            let mut resolved = config.clone();
            resolved.apply_active_profile();
            pool::PoolDefaults {
                provider: resolved.provider,
                model: resolved.model,
                base_url: resolved.base_url,
                api_key: resolved.api_key,
                thinking: resolved.thinking,
                reasoning_effort: resolved.reasoning_effort,
            }
        };
        let input = pool::UpsertInput {
            name: name.clone(),
            provider: provider.clone(),
            model: model.clone(),
            base_url,
            api_key,
            thinking,
            reasoning_effort,
        };
        // 规则在插件里（空字段继承 / 空 key 保留 / 非空 name 立即激活），
        // 宿主只执行它要求的副作用。
        let effects = if name.is_empty() {
            let mut writable = pool::PoolDefaults {
                provider: config.provider.clone(),
                model: config.model.clone(),
                base_url: config.base_url.clone(),
                api_key: config.api_key.clone(),
                thinking: config.thinking,
                reasoning_effort: config.reasoning_effort,
            };
            let effects = pool::update_defaults(&mut writable, &input);
            // 把改动搬回配置（视图只覆盖参与规则的那几项）。
            config.provider = writable.provider;
            config.model = writable.model;
            config.base_url = writable.base_url;
            config.api_key = writable.api_key;
            config.thinking = writable.thinking;
            config.reasoning_effort = writable.reasoning_effort;
            effects
        } else {
            // 分离借用两个字段（不是整个 `config`）：`PoolState` 只需要池与
            // 激活指针，多借用一项都会把宿主类型带进插件。经 `DerefMut`
            // 一次拿到底再解构，避免两次 `&mut config.field` 的整锁借用冲突。
            let crate::config::AgentConfig {
                api_profiles,
                active_api,
                ..
            } = &mut *config;
            let mut state = pool::PoolState {
                profiles: api_profiles,
                active_api,
            };
            pool::upsert(&mut state, &defaults, input)
        };
        let cfg_snapshot = config.clone();
        drop(config);

        for effect in effects {
            match effect {
                pool::PoolEffect::RebuildProvider => {
                    if self.rebuild_provider(&cfg_snapshot).await {
                        self.set_model(model.clone()).await;
                    }
                }
                pool::PoolEffect::BroadcastApiConfig => {}
                pool::PoolEffect::Notice(_) | pool::PoolEffect::Persist
                | pool::PoolEffect::BroadcastTeams => {}
            }
        }
        // 回执文案由插件给（它知道改的是什么），宿主只补"生效的是哪个模型"。
        let label = if name.is_empty() {
            format!("default API updated: {provider} / {model}")
        } else {
            format!("API saved and activated: {name} ({provider} / {model})")
        };
        self.emit(BackendEvent::Error {
            session_id: None,
            message: label,
        });
        // 先落盘、再广播：emit_api_config 以共享存储为准，必须等本次变更写入
        // core.toml 后发事件，否则面板读到旧列表。
        self.persist_config(&cfg_snapshot).await;
        self.emit_api_config().await;
    }

    /// 切换到某个 profile（空 = 顶层默认）。
    ///
    /// **规则在插件里**（`echo_plugin_provider_llm::admin::switch`）：找不到
    /// 就明确报错、空名清空 active。宿主执行它要求的副作用（重建 provider /
    /// 落盘 / 广播）。
    pub(crate) async fn switch_api(&self, name: &str) {
        use echo_plugin_provider_llm::admin as pool;
        let mut config = self.model.config.write().await;
        let effects = {
            let crate::config::AgentConfig {
                api_profiles,
                active_api,
                ..
            } = &mut *config;
            pool::switch(
                &mut pool::PoolState {
                    profiles: api_profiles,
                    active_api,
                },
                name,
            )
        };
        let effects = match effects {
            Ok(effects) => effects,
            Err(message) => {
                drop(config);
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message,
                });
                return;
            }
        };
        let cfg_snapshot = config.clone();
        let mut resolved = cfg_snapshot.clone();
        resolved.apply_active_profile();
        let model = resolved.model.clone();
        drop(config);

        for effect in effects {
            match effect {
                pool::PoolEffect::RebuildProvider => {
                    if self.rebuild_provider(&cfg_snapshot).await {
                        self.set_model(model.clone()).await;
                    }
                }
                pool::PoolEffect::Notice(message) => {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    });
                }
                pool::PoolEffect::Persist
                | pool::PoolEffect::BroadcastApiConfig
                | pool::PoolEffect::BroadcastTeams => {}
            }
        }
        self.persist_config(&cfg_snapshot).await;
        self.emit_api_config().await;
    }

    /// 删除一个 profile；若删的是激活项，`active_api` 清空（回落顶层默认）。
    ///
    /// **规则在插件里**（`echo_plugin_provider_llm::admin::delete`）：池里本来
    /// 就没有时只回"not found"，宿主据此**不落盘、不重建 provider**（零写入）。
    pub(crate) async fn delete_api(&self, name: &str) {
        use echo_plugin_provider_llm::admin as pool;
        let mut config = self.model.config.write().await;
        let effects = {
            let crate::config::AgentConfig {
                api_profiles,
                active_api,
                ..
            } = &mut *config;
            pool::delete(
                &mut pool::PoolState {
                    profiles: api_profiles,
                    active_api,
                },
                name,
            )
        };
        // 只有"真的删掉了"才需要落盘 / 重建（`delete` 在未命中时只回 Notice）。
        let removed = effects
            .iter()
            .any(|effect| matches!(effect, pool::PoolEffect::RebuildProvider));
        let cfg_snapshot = config.clone();
        drop(config);

        if !removed {
            for effect in effects {
                if let pool::PoolEffect::Notice(message) = effect {
                    self.emit(BackendEvent::Error {
                        session_id: None,
                        message,
                    });
                }
            }
            return;
        }
        if self.rebuild_provider(&cfg_snapshot).await {
            self.set_model(cfg_snapshot.model.clone()).await;
        }
        for effect in effects {
            if let pool::PoolEffect::Notice(message) = effect {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message,
                });
            }
        }
        self.persist_config(&cfg_snapshot).await;
        self.emit_api_config().await;
    }

    /// Atomically rename an API profile (2026-10).
    ///
    /// Rewrites every by-name reference:
    /// 1. `[agent].api_profiles` 条目名 + `[agent].active_api`；
    /// 2. `[agent].teams[*].api_profile`（persona 引用，含未运行/禁用）；
    /// 3. 运行中 agent 的 persona 引用与计量归属（重建 provider——名字在
    ///    provider 构建时烘焙）；
    /// 4. 会话级覆盖（`session_providers` 重绑）；
    /// 5. 运行中 persona 的工作区对话引用。
    ///
    /// 一致性：先在克隆上算全新状态 → **单次落盘**（失败即中止；内存从未
    /// 提交 = 天然回滚）→ 提交内存 → 运行时传播（失败只告警——持久化已
    /// 正确，最差重启自愈）。
    ///
    /// 成功回 `ApiConfigUpdated` + `ApiProfilesUpdated`（teams 有变更时补
    /// `TeamsList`）；失败回 `Error` 且零写入。
    pub async fn rename_api(
        &self,
        from: &str,
        to: &str,
        admin: Option<&Arc<dyn crate::agent::registry::TeamAdmin>>,
    ) {
        use echo_plugin_provider_llm::admin as pool;
        // 命名规则在插件里（非空 / ≤128 / 无控制字符）。
        if let Err(message) = pool::validate_profile_name(to) {
            self.emit(BackendEvent::Error {
                session_id: None,
                message,
            });
            return;
        }
        let to = to.trim();

        let mut config = self.model.config.write().await;
        let old_active = config.active_api.clone();
        // 改名计划在**克隆上**算（插件给规则：源精确匹配、目标不冲突、同名
        // 幂等、连带改 active）。内存不动，直到落盘成功。
        let plan = {
            let crate::config::AgentConfig {
                api_profiles,
                active_api,
                ..
            } = &mut *config;
            pool::plan_rename(
                &pool::PoolState {
                    profiles: api_profiles,
                    active_api,
                },
                from,
                to,
            )
        };
        let plan = match plan {
            Ok(plan) => plan,
            Err(message) => {
                drop(config);
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message,
                });
                return;
            }
        };
        if from == to {
            // 幂等 no-op：零写入，只回推（Panel 依赖回推收起表单）。
            drop(config);
            self.emit_api_config().await;
            return;
        }

        let mut new_cfg = config.clone();
        new_cfg.api_profiles = plan.profiles;
        new_cfg.active_api = plan.active_api;
        // teams 引用（含未运行/禁用 persona）：落盘前计算改写范围。
        // 管理面由**进程级处理器**注入（`process_commands`）——`Agent` 自己
        // 不再持有它（S4c：团队/全局配置的增删改都不经人格）。
        // 未接线（单测 / 无 manager）时按"无其他人格引用"处理：落盘照旧，
        // 只是不广播 `TeamsList`。这不是静默降级——人名引用与持久化无关。
        let changed_teams: Vec<String> = admin
            .map(|admin| admin.api_ref_owners(from))
            .unwrap_or_default();

        // 单次落盘：池 + active_api + teams 引用同一 patch（见 try_persist_rename）。
        if let Err(e) = self.try_persist_rename(&new_cfg, to, &changed_teams).await {
            drop(config);
            self.emit(BackendEvent::Error {
                session_id: None,
                message: format!("API rename failed: {e}"),
            });
            return;
        }

        // 落盘成功 → 提交内存。
        *config = new_cfg.clone();
        drop(config);
        if let Some(admin) = admin {
            admin.replace_api_refs(from, to);
        }

        // 运行时传播（持久化已正确；单点失败仅告警，重启自愈）。
        self.propagate_api_rename(from, to, &old_active, &new_cfg)
            .await;

        self.emit(BackendEvent::Error {
            session_id: None,
            message: format!("renamed API config: {from} → {to}"),
        });
        self.emit_api_config().await;
        if !changed_teams.is_empty() {
            self.emit_teams_list().await;
        }
    }

    /// 改名后的运行时传播（2026-10）：重建持旧名 provider 的包装器（计量
    /// 归属名在构建时烘焙，改名后不重建会继续按旧名记账），并重绑会话覆盖
    /// 与工作区对话引用。
    ///
    /// 只处理**运行中** agent；未运行 persona 的引用已由磁盘 patch 修正，
    /// 启用/重启后自然生效。
    async fn propagate_api_rename(
        &self,
        from: &str,
        to: &str,
        old_active: &str,
        new_cfg: &AgentConfig,
    ) {
        let active_renamed = old_active == from;
        // 1) 本 agent（管理面/进程级核心服务代理）。
        self.rebind_renamed_profile(from, to, active_renamed, new_cfg)
            .await;
        let _ = self.rebind_session_providers(from, to).await;
        if let Some(store) = self.workspace_store() {
            if let Err(e) = store.rename_api_profile(from, Some(to)) {
                tracing::warn!(error = %e, %from, %to, "workspace api ref rewrite failed");
            }
        }
        // 2) 全部运行中 persona（与 ClearHistory 的枚举口径一致）。
        if let Some(mgr) = self.registry() {
            for running in mgr.all() {
                let agent = &running.agent;
                agent
                    .rebind_renamed_profile(from, to, active_renamed, new_cfg)
                    .await;
                agent.rebind_session_providers(from, to).await;
                if let Some(store) = agent.workspace_store() {
                    match store.rename_api_profile(from, Some(to)) {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(
                            team = %running.id,
                            count = n,
                            "workspace api refs rewritten after rename"
                        ),
                        Err(e) => tracing::warn!(
                            team = %running.id,
                            error = %e,
                            "workspace api ref rewrite failed"
                        ),
                    }
                }
            }
        }
    }

    /// Test connectivity of an API config by sending a minimal probe request.
    ///
    /// `name` empty tests the active (top-level) config; otherwise the named
    /// profile's values are used. Emits `BackendEvent::ApiTestResult`; the
    /// active provider is never modified.
    pub(crate) async fn test_api_config(&self, name: &str) {
        let timestamp_start = std::time::Instant::now();
        let config = self.model.config.read().await.clone();

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
        // 手动「查余额」与周期快照共用同一把锁（2026-10 审计 P2）：慢查询
        // 叠加本轮周期 tick 会造成同秒重复采样。
        let _snapshot_guard = self.model.balance_snapshot_lock.lock().await;
        let config = self.model.config.read().await.clone();
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
        let config = self.model.config.read().await.clone();
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
                        slice.cache_hit_tokens,
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
                        cache_hit_tokens: slice.cache_hit_tokens,
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
        let cancel = self.turns.cancel.clone();
        tokio::spawn(async move {
            let period_secs = agent.model.config.read().await.balance_snapshot_secs;
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
        // 周期任务与手动「查余额」共用锁（2026-10 审计 P2）：慢查询叠加
        // 本轮周期 tick 会造成同秒重复采样——try_lock 失败（手动查询正在
        // 进行）时跳过本轮，下个 tick 再采。
        let Ok(_snapshot_guard) = self.model.balance_snapshot_lock.try_lock() else {
            tracing::debug!("balance snapshot skipped (manual balance query in flight)");
            return;
        };
        let config = self.model.config.read().await.clone();
        let targets = metrics_targets(&config);
        // 多个 profile 时并发查询（tokio::join!）——此前串行，多 profile
        // 时一轮总耗时 = Σ（逐条查询 + 网络延迟），反而拉长本轮窗口。
        let probes: Vec<_> = targets
            .iter()
            .filter_map(|target| {
                let probe = resolve_probe_config(&config, target).ok()?;
                if deepseek_balance_endpoint(&probe.base_url).is_none()
                    || probe.effective_api_key().is_empty()
                {
                    return None;
                }
                Some((target, probe))
            })
            .collect();
        let futures = probes
            .iter()
            .map(|(_, probe)| fetch_balance(probe))
            .collect::<Vec<_>>();
        for ((target, _), result) in probes.iter().zip(futures) {
            let fetched = result.await;
            match fetched {
                Ok(fetched) => store.record_balance(
                    target,
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
        let store = match self.model.config_store.lock().await.clone() {
            Some(s) => s,
            None => return,
        };
        let text = text.to_string();
        if let Err(error) = store.patch(|root| {
            let plugins = echo_config::ensure_table(root, "plugins");
            let system_prompt = echo_config::ensure_table(plugins, "system_prompt");
            system_prompt.insert("text".into(), toml::Value::String(text));
            Ok(())
        }) {
            tracing::warn!(error = %error, "failed to persist system prompt plugin");
        }
    }

    /// Persist a config snapshot's `[agent]` section back to the TOML file
    /// through the shared [`echo_config::ConfigStore`].
    pub(crate) async fn persist_config(&self, cfg: &AgentConfig) {
        let store = match self.model.config_store.lock().await.clone() {
            Some(s) => s,
            None => return,
        };
        tracing::debug!(api_key_len = cfg.api_key.len(), "persisting agent config");
        if let Err(e) = store.patch(|root| patch_agent_table(root, cfg)) {
            tracing::warn!(error = %e, "failed to persist agent config");
            return;
        }
        tracing::info!(path = %store.path().display(), "agent config persisted");
    }

    /// 改名专用落盘：供应商池 / `active_api` / `teams[*].api_profile` 引用
    /// 在**同一次** [`ConfigStore::patch`] 内改写（write_lock + tmp→rename
    /// 原子替换）——两条写路径合一时序，杜绝"池已改名、teams 未改"的
    /// 崩溃窗口（该窗口内 persona 引用悬空、会静默回退全局默认）。
    ///
    /// 无 ConfigStore（单测 / 嵌入场景）→ `Ok`，由调用方只改内存。
    async fn try_persist_rename(
        &self,
        cfg: &AgentConfig,
        to: &str,
        changed_team_ids: &[String],
    ) -> Result<(), String> {
        let store = match self.model.config_store.lock().await.clone() {
            Some(s) => s,
            None => return Ok(()),
        };
        let to_owned = to.to_string();
        let ids = changed_team_ids.to_vec();
        store
            .patch(move |root| {
                patch_agent_table(root, cfg)?;
                // 磁盘 teams 表已由 patch_agent_table 原样带回——在此基础上
                // 只改引用名（直接改字段，避免重序列化 TeamMember 的口径分叉）。
                if let Some(agent) = root.get_mut("agent").and_then(|v| v.as_table_mut()) {
                    if let Some(teams) = agent.get_mut("teams").and_then(|v| v.as_table_mut()) {
                        for id in &ids {
                            if let Some(entry) = teams.get_mut(id).and_then(|v| v.as_table_mut()) {
                                entry.insert(
                                    "api_profile".into(),
                                    toml::Value::String(to_owned.clone()),
                                );
                            }
                        }
                    }
                }
                Ok(())
            })
            .map_err(|e| format!("persist error: {e}"))
    }
}

/// 把 `cfg` 快照序列化进 `root["agent"]`（改名与常规持久化共用，保证
/// 「api_key 保留」「teams/disabled_teams 以磁盘为准」两条口径不漂移）。
///
/// - `api_key` 排除在 `AgentConfig` 泛型序列化之外：显式键非空时写入该值，
///   否则保留磁盘现值——任何无关保存都不得把文件里的 key 抹掉；
/// - `teams` / `disabled_teams` 由 AgentManager 独立持久化：快照里的副本
///   可能已过期，一律以磁盘为准，避免"某次无关保存把人格名单写回旧值"。
fn patch_agent_table(root: &mut toml::Table, cfg: &AgentConfig) -> Result<(), String> {
    let mut value =
        toml::Value::try_from(cfg).map_err(|e| format!("serialize agent config: {e}"))?;
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
    root.insert("agent".into(), value);
    Ok(())
}

/// 一次余额查询的结果（金额为原始字符串，如 `"1287.91"`）。
///
/// 实现归 provider 插件（[`echo_plugin_provider_llm::balance`]）：端点归一
/// 与响应字段是**供应商的知识**。这里只留一个适配器——把宿主的
/// `AgentConfig` 投影成插件认识的 [`echo_plugin_provider_llm::ProviderConfig`]。
pub(crate) use echo_plugin_provider_llm::balance::BalanceFetch;

/// 请求 DeepSeek `/user/balance`（15s 超时；无事件 / 记录副作用）。
pub(crate) async fn fetch_balance(probe: &AgentConfig) -> Result<BalanceFetch, String> {
    let projection = echo_plugin_provider_llm::ProviderConfig {
        provider: probe.provider.clone(),
        base_url: probe.effective_base_url(),
        model: probe.model.clone(),
        api_key: probe.effective_api_key(),
        thinking: probe.thinking,
        reasoning_effort: probe.reasoning_effort,
    };
    echo_plugin_provider_llm::balance::fetch_balance(&projection).await
}

/// 宽松解析金额字符串（失败 = 0.0；不阻断快照记录）。
pub(crate) use echo_plugin_provider_llm::balance::parse_amount;

/// 从 base_url 推导 DeepSeek 余额查询端点。
pub(crate) use echo_plugin_provider_llm::balance::deepseek_balance_endpoint;

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

#[cfg(test)]
mod tests {
    use super::*;

    struct StubProvider;

    #[async_trait::async_trait]
    impl crate::llm::LlmProvider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }
        fn default_model(&self) -> &str {
            "stub"
        }
        async fn chat(
            &self,
            _request: &crate::llm::ChatRequest,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Config("stub".into()))
        }
        async fn chat_stream(
            &self,
            _request: &crate::llm::ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<crate::llm::ChatChunk>,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Config("stub".into()))
        }
    }

    /// 改名单 patch：池 + active_api + teams 引用在**同一次** `store.patch`
    /// 内改写；磁盘 api_key 保留；无关 team 引用不被触碰。
    #[tokio::test]
    async fn try_persist_rename_writes_pool_and_teams_in_single_patch() {
        let tmp = std::env::temp_dir().join(format!(
            "echo-rename-patch-{}-{}.toml",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(
            &tmp,
            "[agent]\nprovider = \"openai\"\napi_key = \"sk-top\"\nactive_api = \"alpha\"\n\n\
             [agent.teams.alice]\nname = \"Alice\"\napi_profile = \"alpha\"\n\n\
             [agent.teams.bob]\nname = \"Bob\"\napi_profile = \"beta\"\n",
        )
        .unwrap();
        let agent = Agent::new(
            Arc::new(StubProvider),
            crate::config::AgentConfig::default(),
            crate::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
        );
        agent.set_config_path(tmp.clone());

        let new_cfg = crate::config::AgentConfig {
            provider: "openai".into(),
            api_key: String::new(), // 空 = 保留磁盘值
            active_api: "gamma".into(),
            api_profiles: vec![crate::config::ApiProfile::new("gamma", "deepseek", "m1")],
            ..Default::default()
        };
        agent
            .try_persist_rename(&new_cfg, "gamma", &["alice".to_string()])
            .await
            .expect("single patch persist must succeed");

        let content = std::fs::read_to_string(&tmp).unwrap();
        assert!(
            content.contains("name = \"gamma\""),
            "pool rewritten: {content}"
        );
        assert!(
            content.contains("active_api = \"gamma\""),
            "active_api rewritten: {content}"
        );
        assert!(
            content.contains("api_key = \"sk-top\""),
            "disk api_key preserved: {content}"
        );
        assert!(
            content.contains("name = \"Alice\""),
            "unrelated team fields preserved: {content}"
        );
        assert!(
            content.contains("api_profile = \"gamma\""),
            "referencing team rewritten: {content}"
        );
        assert!(
            content.contains("api_profile = \"beta\""),
            "non-referencing team untouched: {content}"
        );
        assert!(
            !content.contains("api_profile = \"alpha\""),
            "old reference gone: {content}"
        );
        std::fs::remove_file(&tmp).ok();
    }
}
