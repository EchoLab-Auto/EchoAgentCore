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
    ///   已卸载的插件）优先于 persona 名单。
    ///
    /// 与旧实现的区别：**逐名双向应用**（取消勾选即禁用、重新勾选即恢复，
    /// 此前只有禁用方向，重勾必须重启才生效），且插件维度只作用于本
    /// persona 的工具/技能注册表（不再驱动共享 registry 的
    /// mount/unmount，避免"单人格改动扩散到全员"）。
    pub async fn apply_capabilities(&self, profile: &crate::config::AgentProfile) {
        *self
            .identity.capabilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(profile.clone());

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
            let mut skills = self.prompt.skills.lock().await;
            for name in skills.names() {
                let allowed = persona_skill_allowed(profile, &name)
                    && !global_disabled_skills.iter().any(|s| s == &name);
                skills.set_enabled(&name, allowed);
            }
        }
        self.prompt.invalidate().await;

        // 3) 门控插件（包/表维度）：仅在"不允许"时禁用——与启动期逐人格
        //    门控同语义；重新允许的恢复由上面的逐名双向步骤完成。全局状态
        //    （注册表）优先：运行期卸载不会被本步骤反向覆盖。
        for plugin_id in self.gated_plugin_ids() {
            let allowed = crate::plugins::profile_allows_plugin(profile, &plugin_id)
                && self.plugin_globally_enabled(&plugin_id);
            if !allowed {
                self.apply_plugin_gating(&plugin_id, false);
            }
        }
    }

    /// 全局禁用列表（工具/技能）：读进程级策略（组合根注入）。
    /// 无策略（单元测试）时回退本 agent 的配置快照。
    async fn global_disabled_lists(&self) -> (Vec<String>, Vec<String>) {
        if let Some(policy) = self.global_policy() {
            return (policy.disabled_tools(), policy.disabled_skills());
        }
        let cfg = self.model.config.read().await;
        (cfg.disabled_tools.clone(), cfg.disabled_skills.clone())
    }

    /// 本 persona 白/黑名单是否允许某插件（capabilities 未设置 = 允许）。
    pub fn persona_allows_plugin(&self, plugin_id: &str) -> bool {
        self.identity.capabilities
            .lock()
            .ok()
            .and_then(|guard| {
                guard
                    .as_ref()
                    .map(|p| crate::plugins::profile_allows_plugin(p, plugin_id))
            })
            .unwrap_or(true)
    }

    /// 受 persona 名单门控的插件 id 集合。
    ///
    /// = 框架内置的固定清单（工具/技能/工作区/subagent/management）
    ///   ∪ **运行期注册的全部 `kind == PluginInstance` 插件**。
    ///
    /// 后者从注册表按 kind 推导，框架因此**不需要认识任何平台**：新增一个
    /// 实例插件（`echo-agent.instance.<platform>`）自动进入门控，无需改
    /// 本 crate 的常量表。
    fn gated_plugin_ids(&self) -> Vec<String> {
        // 读**本 agent 注入的**宿主，不读进程级锚点（2026-10 技术债清理）：
        // 字段是必填槽、组合根注入的就是同一实例，语义等价；但依赖变为显式，
        // 且同一进程的两套宿主不会互相看见对方的注册表。
        let registered: Vec<(String, String)> = self
            .plugin_host()
            .registry
            .descriptors()
            .into_iter()
            .map(|d| (d.id, d.kind))
            .collect();
        let mut ids = gated_plugin_ids_from(&registered);
        // 补充：**工具/技能注册表里实际出现的包 id**。
        //
        // 为什么需要这一句：`Agent` 在插件**挂载之前**就构造好了（`make_agent`
        // 早于 `mount_plan`），那时 `descriptors()` 是空的——只按注册表派生会让
        // "包维度门控"在启动窗口里完全失效（名单外的人格照样拿到 workspace 的
        // 工具），直到插件挂载后 `reapply_plugin_gating` 才补上。
        //
        // 包标签在**工具注册时**就打好了（组合根装配期，早于挂载），所以这里
        // 能拿到真实存在的包；由此门控不依赖挂载顺序。
        for name in self.tools.names() {
            if let Some(package) = self.tools.package_of(&name) {
                if !ids.contains(&package) {
                    ids.push(package);
                }
            }
        }
        if let Ok(skills) = self.prompt.skills.try_lock() {
            for package in skills.packages() {
                if !ids.contains(&package) {
                    ids.push(package);
                }
            }
        }
        ids
    }

    /// 当前 persona 门控集合的快照（诊断用：启动日志、面板、测试）。
    ///
    /// 暴露它是因为这个集合的**完整性与启动顺序有关**：`Agent` 早于插件挂载
    /// 构造（`make_agent` → `mount_plan`），那一刻注册表里没有 descriptor。
    /// 集合因此还有第二个来源——工具/技能的**包标签**（注册时打好，早于挂载）。
    /// 想知道"某个 persona 到底会被哪些插件门控"，直接看这里，不要猜。
    pub fn gated_plugin_ids_snapshot(&self) -> Vec<String> {
        self.gated_plugin_ids()
    }

    /// 共享注册表中该插件当前是否启用（无全局锚点 = true，以名单为准）。
    fn plugin_globally_enabled(&self, plugin_id: &str) -> bool {
        // 同上：`is_enabled` 对未注册 id 返回 true，与旧 `None => true` 等价。
        self.plugin_host().registry.is_enabled(plugin_id)
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
                .identity.capabilities
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
                .identity.capabilities
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(|p| persona_skill_allowed(p, name)))
                .unwrap_or(true);
        let ok = {
            let mut skills = self.prompt.skills.lock().await;
            skills.set_enabled(name, allowed)
        };
        if ok {
            self.prompt.invalidate().await;
        }
        ok
    }

    /// Persona 级 API：设置本 agent 对全局供应商池的引用（不重建）。
    /// `None` = 跟随全局默认配置；`Some(name)` = 使用池中该 profile。
    pub async fn set_persona_api(&self, name: Option<String>) {
        *self.model.persona_api.write().await = name;
    }

    /// 同步版 [`Self::set_persona_api`]：供组合根启动期（make_agent 为同步
    /// 闭包）调用；启动期无人持有读锁，try_write 必然成功。
    pub fn set_persona_api_now(&self, name: Option<String>) {
        if let Ok(mut slot) = self.model.persona_api.try_write() {
            *slot = name;
        }
    }

    /// 当前 persona 级的 API 供应商引用。
    pub async fn persona_api(&self) -> Option<String> {
        self.model.persona_api.read().await.clone()
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
        let reference = self.model.persona_api.read().await.clone();
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
            let mut cfg = self.model.config.write().await;
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
        *self.model.provider.write().await = provider;
        self.set_model(resolved.model.clone()).await;
        true
    }
    /// API profile 改名传播（2026-10）：本 agent 的 persona 引用或全局激活名
    /// 命中被改名的 profile 时，重建 provider——计量归属名是在构建
    /// provider 时**烘焙**进包装器的（见 `wrap_metering`），只改配置字符串
    /// 而不重建会继续按旧名记账。
    ///
    /// - persona 引用命中 → 重引用为新名 + `apply_persona_api`（按名解析）；
    /// - 跟随全局默认（无引用）且默认激活名被改写 → `rebuild_provider`
    ///   （只重建 provider、不动本 agent 配置字段）。
    ///
    /// 调用方须已把 `global`（含改名后的池）传入；未命中的 agent 是 no-op。
    pub(crate) async fn rebind_renamed_profile(
        &self,
        from: &str,
        to: &str,
        active_renamed: bool,
        global: &AgentConfig,
    ) {
        let reference = self.model.persona_api.read().await.clone();
        match reference.as_deref().filter(|n| !n.is_empty()) {
            Some(name) if name == from => {
                // persona 引用命中：跟随改名重引用并重建（计量名 → 新名）。
                *self.model.persona_api.write().await = Some(to.to_string());
                self.apply_persona_api(global).await;
            }
            Some(_) => {
                // 引用其他 profile：解析值与计量名都不受影响 → 无需重建。
            }
            None => {
                if active_renamed {
                    // 跟随全局默认：重建以把计量归属切到新名。
                    self.rebuild_provider(global).await;
                }
            }
        }
    }

    /// 插件启停对本 agent 注册表的批量效果（**包维度，横跨工具与技能**）。
    ///
    /// Package 是横跨 plugin + tool + skill 的标签：本方法把一次包级
    /// 启停传播到本 agent 的两类注册表——
    /// - 工具：`ToolRegistry::set_package_enabled`（按工具的 package 标签）；
    /// - 技能：`SkillRegistry::set_package_enabled`（按 `SKILL.md` 的
    ///   `package:` frontmatter）——`skills.dir` 插件为**整表**语义，
    ///   其余包按同名 package 精确匹配（如 QQ 包：qq-management /
    ///   qq-transport 随 `echo-agent.instance.qq` 一起启停）。
    ///
    /// 启用方向会按本 persona 的工具/技能名单**收紧**：整包启用不越过
    /// 白/黑名单（名单外的成员保持禁用），与启动期"名单先应用、包后禁用"
    /// 的组合语义一致。
    pub fn apply_plugin_gating(&self, plugin_id: &str, enabled: bool) {
        // 一次性判定「该插件的最终启用态是否真的变了」：本函数在每次
        // `apply_capabilities`（插件目录 5s 热重载 / 名单变更 / 能力重算）里
        // 都会被逐 persona 调用，而绝大多数调用的结果是幂等的——按"每次调用
        // 都打日志"记，实测 321 条/天（工具 + 技能各一条 × N 个 persona），
        // 真正的开关动作被淹没。改为只在状态翻转时记录。
        let changed = self.note_gating_state(plugin_id, enabled);
        // 工具维度：按 package 标签批量启停。
        let affected_tools = self.tools.set_package_enabled(plugin_id, enabled);
        if enabled {
            self.tighten_tools_by_lists(plugin_id);
        }
        if changed && affected_tools > 0 {
            tracing::info!(
                plugin = plugin_id,
                enabled,
                affected = affected_tools,
                "plugin tool package toggled"
            );
        }

        // 技能维度：Package 标签横跨技能，同名包内技能随包启停。
        // （锁竞争时跳过——下一周期/切换时再应用。）
        if let Ok(mut skills) = self.prompt.skills.try_lock() {
            let cap = self.identity.capabilities.lock().ok().and_then(|g| g.clone());
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
            if changed && affected_skills > 0 {
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

    /// 记录插件的最终启用态；返回 `true` 表示相对上次记录发生了变化。
    ///
    /// 首次记录即「变化」（启动期的那次应用值得留一条日志）。
    fn note_gating_state(&self, plugin_id: &str, enabled: bool) -> bool {
        match self.instrumentation.gating_log_state.insert(plugin_id.to_string(), enabled) {
            Some(previous) => previous != enabled,
            None => true,
        }
    }

    /// 按本 persona 的工具名单，把包内"名单外"的成员重新禁用。
    /// 同步尽力（try_write）；未配置 capabilities 时为 no-op。
    fn tighten_tools_by_lists(&self, package: &str) {
        let Some(cap) = self.identity.capabilities.lock().ok().and_then(|g| g.clone()) else {
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

    /// Whether a *dynamically registered* tool is allowed for this agent
    /// (allowlist first, denylist refinement; empty allowlist = all allowed).
    ///
    /// **不再有按工具名的特判**（2026-11 S7）：`spawn_subagent` 曾在装配期
    /// 之外靠一条名字分支回答"subagent 插件装了没"——现在它就是一个注册表
    /// 工具、带着 `echo-agent.subagent` 包标签，插件门控经
    /// `set_package_enabled` 直接作用在注册表上（见 `apply_plugin_gating`）。
    ///
    /// 本方法留着是为了**名单**语义（persona 的 tool 白/黑名单），与插件门控
    /// 是两件事。
    pub fn allows_dynamic_tool(&self, name: &str) -> bool {
        let guard = self
            .identity.capabilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// 从注册表快照派生 persona 门控 id 集合。
///
/// `registered` 是 `(插件 id, kind 标签)`。判定规则只有一条：**该 kind 的
/// 作用域是不是 `Persona`**（见 [`crate::plugins::scope::KIND_SCOPE`]）。
///
/// ## 为什么不再有固定清单
///
/// 此前这里是「`GATED_PLUGIN_IDS` 硬编码 5 项 ∪ 按 `kind == "instance"` 派生」的
/// 双轨逻辑——同一件事两种判据，而固定清单那一轨已经错过三轮（见
/// `plugins/scope.rs` 模块文档的表格）。现在两条轨并成一条：`kind` 定作用域，
/// 新增 `tool`/`skill` 插件**不需要改任何清单**。
///
/// 硬编码字符串的老坑也随之消失：`2d46b7d` 把 wire 名从 `adapter` 改成
/// `instance` 时，这里曾因比较字面量而让实例插件永远进不了门控集合。
fn gated_plugin_ids_from(registered: &[(String, String)]) -> Vec<String> {
    crate::plugins::persona_scoped_ids(registered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(id: &str, kind: &str) -> (String, String) {
        (id.to_string(), kind.to_string())
    }

    /// 实例插件必须进 persona 门控集合（按 wire kind 名推导，不认平台名）。
    ///
    /// 回归背景（2026-10 审计发现）：`2d46b7d` 把实例插件的
    /// wire 名从 `adapter` 改成 `instance`，而本函数的字符串比较当时硬编码了
    /// `"adapter"`，于是实例插件**永远进不了**门控集合——`enabled_plugins` 白名单
    /// 管不住 `instance.qq`，白名单形同虚设且无任何测试覆盖。
    #[test]
    fn registered_instance_plugins_join_the_gated_set() {
        let ids = gated_plugin_ids_from(&[desc("echo-agent.instance.qq", "instance")]);
        assert!(
            ids.iter().any(|id| id == "echo-agent.instance.qq"),
            "实例插件必须进门控集合，实际: {ids:?}"
        );
    }

    /// 新增平台不改框架：任意 `instance.*` id 都自动进门控，无需登记常量。
    #[test]
    fn any_instance_plugin_is_gated_without_framework_changes() {
        let ids = gated_plugin_ids_from(&[
            desc("echo-agent.instance.qq", "instance"),
            desc("echo-agent.instance.telegram", "instance"),
        ]);
        assert!(ids.iter().any(|id| id == "echo-agent.instance.telegram"));
    }

    /// 非实例类插件不进 persona 门控集合（loop/provider 等由各自机制管理）。
    #[test]
    fn non_instance_plugins_stay_out_of_the_gated_set() {
        let ids = gated_plugin_ids_from(&[
            desc("echo-agent.loop.single", "loop"),
            desc("echo-agent.provider.llm", "provider"),
            desc("echo-agent.management.panel", "management"),
        ]);
        for id in ["echo-agent.loop.single", "echo-agent.provider.llm"] {
            assert!(!ids.contains(&id.to_string()), "{id} 不该被 persona 门控");
        }
    }

    /// 能力型插件全部在内，且不发生重复。
    ///
    /// 取代原先那条「框架固定清单始终在内」的测试：现在没有固定清单了，
    /// 判据是 kind。重复项的问题也随之消失——派生只遍历注册表一次。
    #[test]
    fn capability_plugins_are_gated_and_deduplicated() {
        let registered = [
            desc("echo-agent.tools.builtin", "tool"),
            desc("echo-agent.skills.dir", "skill"),
            desc("echo-agent.instance.qq", "instance"),
        ];
        let ids = gated_plugin_ids_from(&registered);
        for id in [
            "echo-agent.tools.builtin",
            "echo-agent.skills.dir",
            "echo-agent.instance.qq",
        ] {
            assert!(ids.iter().any(|x| x == id), "{id} 应在门控集合里");
        }
        assert_eq!(ids.len(), registered.len(), "派生不得重复或凭空加项");
    }

    /// 同一能力型插件既有真身又出现在注册表里时，集合里仍只有一项。
    ///
    /// 老机制用 `!ids.contains(id)` 手工去重（因为固定清单与注册表是两份来源）；
    /// 现在只有一份来源，但这条断言保留——它守的是"派生实现真的只遍历一次"，
    /// 将来若有人重新引入第二份来源，这里会红。
    #[test]
    fn a_plugin_appearing_once_is_listed_once() {
        let ids = gated_plugin_ids_from(&[desc("echo-agent.shell", "tool")]);
        assert_eq!(ids, vec!["echo-agent.shell".to_string()]);
    }
}
