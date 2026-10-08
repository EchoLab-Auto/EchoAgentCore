//! 能力门控（gating）：插件/工具/技能三层白名单与全局禁用的应用。
//!
//! 从 `agent/mod.rs` 拆出（框架优化）：`Agent` 的 capabilities 应用
//! （apply_capabilities）、per-plugin 门控（apply_plugin_gating /
//! reapply_*）、包级收紧（tighten_tools_by_lists）、动态工具可见性
//! （allows_dynamic_tool）与 persona 名单纯函数（persona_*_allowed）。
//! 纯移动零行为变化。

use super::*;

impl Agent {
    /// Apply per-persona capability configuration（运行期热更新入口）。
    ///
    /// Semantics:
    /// - `enabled_*` allowlists: non-empty => only the listed plugins/tools/
    ///   skills are visible to this agent; empty => everything is available.
    /// - `disabled_*` denylists refine afterwards (keeps old configs working).
    /// - 全局禁用（`[agent].disabled_tools/disabled_skills`、共享注册表中被
    ///   `TogglePlugin` 卸载的插件）优先于 persona 名单。
    ///
    /// 与旧实现的区别：**逐名双向应用**（取消勾选即禁用、重新勾选即恢复，
    /// 此前只有禁用方向，重勾必须重启才生效），且插件维度只作用于本
    /// persona 的工具/技能注册表（不再驱动共享 registry 的
    /// mount/unmount，避免"单人格改动扩散到全员"）。
    pub async fn apply_capabilities(&self, profile: &crate::config::AgentProfile) {
        *self.capabilities.lock().unwrap() = Some(profile.clone());

        // 全局禁用列表：优先管理面（默认 agent）的实时配置——ToggleTool /
        // ToggleSkill 只更新管理面配置，其余 persona 的启动快照会过期。
        let (global_disabled_tools, global_disabled_skills) = self.global_disabled_lists().await;

        // 1) 工具逐名双向：白名单 ∧ 非黑名单 ∧ 非全局禁用。
        for name in self.tools.names() {
            let allowed = persona_tool_allowed(profile, &name)
                && !global_disabled_tools.iter().any(|t| t == &name);
            self.tools.set_enabled(&name, allowed).await;
        }

        // 2) 技能逐名双向：同一语义。
        {
            let mut skills = self.skills.lock().await;
            for name in skills.names() {
                let allowed = persona_skill_allowed(profile, &name)
                    && !global_disabled_skills.iter().any(|s| s == &name);
                skills.set_enabled(&name, allowed);
            }
        }
        *self.system_prompt_cache.write().await = None;

        // 3) 门控插件（包/表维度）：仅在"不允许"时禁用——与启动期逐人格
        //    门控同语义；重新允许的恢复由上面的逐名双向步骤完成。全局状态
        //    （注册表）优先：运行期 TogglePlugin 的卸载不会被本步骤反向覆盖。
        for plugin_id in crate::plugins::GATED_PLUGIN_IDS {
            let allowed = crate::plugins::profile_allows_plugin(profile, plugin_id)
                && self.plugin_globally_enabled(plugin_id);
            if !allowed {
                self.apply_plugin_gating(plugin_id, false);
            }
        }
    }

    /// 全局禁用列表（工具/技能）：读进程级策略（组合根注入）。
    /// 无策略（单元测试）时回退本 agent 的配置快照。
    async fn global_disabled_lists(&self) -> (Vec<String>, Vec<String>) {
        if let Some(policy) = global_policy() {
            return (policy.disabled_tools(), policy.disabled_skills());
        }
        let cfg = self.config.read().await;
        (cfg.disabled_tools.clone(), cfg.disabled_skills.clone())
    }

    /// 本 persona 白/黑名单是否允许某插件（capabilities 未设置 = 允许）。
    pub fn persona_allows_plugin(&self, plugin_id: &str) -> bool {
        self.capabilities
            .lock()
            .ok()
            .and_then(|guard| {
                guard
                    .as_ref()
                    .map(|p| crate::plugins::profile_allows_plugin(p, plugin_id))
            })
            .unwrap_or(true)
    }

    /// 共享注册表中该插件当前是否启用（无全局锚点 = true，以名单为准）。
    fn plugin_globally_enabled(&self, plugin_id: &str) -> bool {
        plugin_host_global()
            .map(|host| host.registry.is_enabled(plugin_id))
            .unwrap_or(true)
    }

    /// 全局插件状态变化后，重新评估该插件在本 persona 的最终效果：
    /// 最终 = 全局启用 ∧ 本 persona 白/黑名单（禁用对全员生效）。
    ///
    /// 全局 mount/unmount 闭包逐 persona 调用，取代旧的无条件批量启停——
    /// 后者会在 mount 时把"名单外"的 persona 一并放开。
    pub fn reapply_plugin_gating(&self, plugin_id: &str, globally_enabled: bool) {
        let allowed = globally_enabled && self.persona_allows_plugin(plugin_id);
        self.apply_plugin_gating(plugin_id, allowed);
    }

    /// 全局工具启停变化后，重新评估本 persona 的最终状态：
    /// 最终 = 全局启用 ∧ 本 persona 名单。返回工具是否存在。
    pub async fn reapply_tool_gating(&self, name: &str, globally_enabled: bool) -> bool {
        let allowed = globally_enabled
            && self
                .capabilities
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(|p| persona_tool_allowed(p, name)))
                .unwrap_or(true);
        self.tools.set_enabled(name, allowed).await
    }

    /// 全局技能启停变化后，重新评估本 persona 的最终状态。返回技能是否存在。
    pub async fn reapply_skill_gating(&self, name: &str, globally_enabled: bool) -> bool {
        let allowed = globally_enabled
            && self
                .capabilities
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(|p| persona_skill_allowed(p, name)))
                .unwrap_or(true);
        let ok = {
            let mut skills = self.skills.lock().await;
            skills.set_enabled(name, allowed)
        };
        if ok {
            *self.system_prompt_cache.write().await = None;
        }
        ok
    }

    /// Persona 级 API：设置本 agent 对全局供应商池的引用（不重建）。
    /// `None` = 跟随全局默认配置；`Some(name)` = 使用池中该 profile。
    pub async fn set_persona_api(&self, name: Option<String>) {
        *self.persona_api.write().await = name;
    }

    /// 同步版 [`Self::set_persona_api`]：供组合根启动期（make_agent 为同步
    /// 闭包）调用；启动期无人持有读锁，try_write 必然成功。
    pub fn set_persona_api_now(&self, name: Option<String>) {
        if let Ok(mut slot) = self.persona_api.try_write() {
            *slot = name;
        }
    }

    /// 当前 persona 级的 API 供应商引用。
    pub async fn persona_api(&self) -> Option<String> {
        self.persona_api.read().await.clone()
    }

    /// Persona 级 API：按引用重建本 agent 的 provider。
    ///
    /// 以传入的**全局配置**（含最新供应商池）为基准：
    /// - `Some(name)` 且池中存在 → 合入该 profile 值（非空覆盖），
    ///   不改变全局 active_api / 顶层字段；
    /// - 其他情况（None 或名字不存在）→ 跟随全局默认（顶层 + active_api）。
    ///
    /// 返回是否成功重建；失败时保留旧 provider 并 emit Error。
    pub async fn apply_persona_api(&self, global: &AgentConfig) -> bool {
        let reference = self.persona_api.read().await.clone();
        let mut resolved = global.clone();
        let ok = match reference.as_deref().filter(|n| !n.is_empty()) {
            Some(name) => resolved.apply_named_profile(name),
            None => {
                resolved.apply_active_profile();
                true
            }
        };
        if !ok {
            self.emit(BackendEvent::Error {
                session_id: None,
                message: format!(
                    "persona API profile not found in pool: {} — falling back to global default",
                    reference.as_deref().unwrap_or_default()
                ),
            });
            resolved.apply_active_profile();
        }
        // 计量归属：persona 引用命中 → 该 profile 名；否则跟随全局默认
        // （与 `QueryApiMetrics` / 面板 profile 卡片的命名空间一致）。
        let profile_key = match reference.as_deref().filter(|n| !n.is_empty()) {
            Some(name) if ok => name.to_string(),
            _ => global.active_api.clone(),
        };
        resolved.api_key = resolved.effective_api_key();
        resolved.base_url = resolved.effective_base_url();
        // 本 persona 的 config 只更新 API 相关字段（保留权限、预算等其余项）。
        {
            let mut cfg = self.config.write().await;
            cfg.provider = resolved.provider.clone();
            cfg.model = resolved.model.clone();
            cfg.base_url = resolved.base_url.clone();
            cfg.api_key = resolved.api_key.clone();
            cfg.thinking = resolved.thinking;
            cfg.reasoning_effort = resolved.reasoning_effort;
        }
        let provider = match crate::llm::create_provider(&resolved) {
            Ok(p) => p,
            Err(e) => {
                self.emit(BackendEvent::Error {
                    session_id: None,
                    message: format!("persona API provider build failed: {e}"),
                });
                return false;
            }
        };
        // 安全（2026-10）：provider 重建一律经脱敏装饰器包装（LLM 请求
        // 出口卡口），并登记该 persona 生效密钥；计量装饰器（token 用量）
        // 一并装配，供余额/用量图表。
        self.register_config_secrets(&resolved);
        let provider: Arc<dyn LlmProvider> = Arc::from(provider);
        let provider = match self.metrics() {
            Some(store) => crate::llm::wrap_metering(provider, store, profile_key),
            None => provider,
        };
        let provider = match self.redactor() {
            Some(redactor) => crate::llm::wrap_redacting(provider, redactor),
            None => provider,
        };
        *self.provider.write().await = provider;
        self.set_model(resolved.model.clone()).await;
        true
    }

    /// 插件启停对本 agent 注册表的批量效果（**包维度，横跨工具与技能**）。
    ///
    /// Package 是横跨 plugin + tool + skill 的标签：本方法把一次包级
    /// 启停传播到本 agent 的两类注册表——
    /// - 工具：`ToolRegistry::set_package_enabled`（按工具的 package 标签）；
    /// - 技能：`SkillRegistry::set_package_enabled`（按 `SKILL.md` 的
    ///   `package:` frontmatter）——`skills.dir` 插件为**整表**语义，
    ///   其余包按同名 package 精确匹配（如 QQ 包：qq-management /
    ///   qq-transport 随 `echo-agent.adapter.qq` 一起启停）。
    ///
    /// 启用方向会按本 persona 的工具/技能名单**收紧**：整包启用不越过
    /// 白/黑名单（名单外的成员保持禁用），与启动期"名单先应用、包后禁用"
    /// 的组合语义一致。
    pub fn apply_plugin_gating(&self, plugin_id: &str, enabled: bool) {
        // 工具维度：按 package 标签批量启停。
        let affected_tools = self.tools.set_package_enabled(plugin_id, enabled);
        if enabled {
            self.tighten_tools_by_lists(plugin_id);
        }
        if affected_tools > 0 {
            tracing::info!(
                plugin = plugin_id,
                enabled,
                affected = affected_tools,
                "plugin tool package toggled"
            );
        }

        // 技能维度：Package 标签横跨技能，同名包内技能随包启停。
        // （锁竞争时跳过——下一周期/切换时再应用。）
        if let Ok(mut skills) = self.skills.try_lock() {
            let cap = self.capabilities.lock().ok().and_then(|g| g.clone());
            let affected_skills = if plugin_id == crate::plugins::SKILLS_DIR_PLUGIN_ID {
                // 技能目录插件：整表启停。启用时只放开名单内的技能。
                let names = skills.names();
                for name in &names {
                    let ok = cap.as_ref().is_none_or(|p| persona_skill_allowed(p, name));
                    skills.set_enabled(name, enabled && ok);
                }
                names.len()
            } else {
                let affected = skills.set_package_enabled(plugin_id, enabled);
                if enabled {
                    // 收紧：包启用不越过 persona 技能名单。
                    if let Some(cap) = cap.as_ref() {
                        for name in skills.package_names(plugin_id) {
                            if !persona_skill_allowed(cap, &name) {
                                skills.set_enabled(&name, false);
                            }
                        }
                    }
                }
                affected
            };
            if affected_skills > 0 {
                if plugin_id == crate::plugins::SKILLS_DIR_PLUGIN_ID {
                    tracing::info!(
                        enabled,
                        affected = affected_skills,
                        "skills dir plugin toggled"
                    );
                } else {
                    tracing::info!(
                        plugin = plugin_id,
                        enabled,
                        affected = affected_skills,
                        "plugin skill package toggled"
                    );
                }
            }
        }
    }

    /// 按本 persona 的工具名单，把包内"名单外"的成员重新禁用。
    /// 同步尽力（try_write）；未配置 capabilities 时为 no-op。
    fn tighten_tools_by_lists(&self, package: &str) {
        let Some(cap) = self.capabilities.lock().ok().and_then(|g| g.clone()) else {
            return;
        };
        if cap.enabled_tools.is_empty() && cap.disabled_tools.is_empty() {
            return;
        }
        for name in self.tools.package_names(package) {
            if !persona_tool_allowed(&cap, &name) {
                self.tools.try_disable(&name);
            }
        }
    }

    /// Whether a dynamic tool is allowed for this agent
    /// (allowlist first, denylist refinement; empty allowlist = all allowed).
    ///
    /// 单会话模式额外隐藏 `spawn_parallel_task`：一个会话一次只处理一件事，
    /// 并行分支与串行准入语义冲突（改用另一个会话 = 另一条并行通道）。
    pub fn allows_dynamic_tool(&self, name: &str) -> bool {
        if name == "spawn_parallel_task" && self.loop_mode() == echo_defs::LoopMode::Single {
            return false;
        }
        // spawn_subagent 由 subagent 插件实化：插件未挂载（未装配运行态）
        // 或 persona 白名单不含该插件时对模型不可见。
        if name == crate::subagent::SPAWN_SUBAGENT_TOOL {
            let attached = self.subagent.read().map(|g| g.is_some()).unwrap_or(false);
            return attached && self.persona_allows_plugin(crate::plugins::SUBAGENT_PLUGIN_ID);
        }
        let guard = self.capabilities.lock().unwrap();
        let Some(cap) = guard.as_ref() else {
            return true;
        };
        if !cap.enabled_tools.is_empty() && !cap.enabled_tools.iter().any(|t| t == name) {
            return false;
        }
        if cap.disabled_tools.iter().any(|t| t == name) {
            return false;
        }
        true
    }
}

/// persona 名单对某工具是否允许（白名单非空 = 仅列出；黑名单命中即拒绝）。
fn persona_tool_allowed(profile: &crate::config::AgentProfile, name: &str) -> bool {
    (profile.enabled_tools.is_empty() || profile.enabled_tools.iter().any(|t| t == name))
        && !profile.disabled_tools.iter().any(|t| t == name)
}

/// persona 名单对某技能是否允许（语义同 [`persona_tool_allowed`]）。
fn persona_skill_allowed(profile: &crate::config::AgentProfile, name: &str) -> bool {
    (profile.enabled_skills.is_empty() || profile.enabled_skills.iter().any(|s| s == name))
        && !profile.disabled_skills.iter().any(|s| s == name)
}
