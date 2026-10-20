//! 供应商池**管理策略**：增删改 / 激活 / 改名，纯数据操作。
//!
//! ## 为什么归本插件
//!
//! provider 工厂在这里，**provider 池的管理规则**也该在这里：空字段从当前
//! 生效配置继承、空 key = 保留原值、删掉激活项要回落到顶层默认、改名要连带
//! 改 `active_api`。这些规则此前散在宿主 `agent/api_admin.rs`（1,208 行）
//! 里，与"持久化 / 重建 provider / 广播"混在一起。
//!
//! 拆开后：**规则在这里**（纯函数，输入输出都是 `ApiProfile`），宿主只做
//! 三件事——读 `AgentConfig` → 调这里 → 写回并落盘 / 重建 / 广播。
//!
//! ## 为什么是"就地编辑一个视图"而不是"返回新配置"
//!
//! 宿主的 `AgentConfig` 有 30+ 字段，其中只有 5 个参与池管理。要求插件认识
//! 整个 `AgentConfig` 会把宿主类型带进插件（全插件化 S3 的失败模式）。
//! [`PoolState`] 是**借用视图**：只暴露参与规则的那几项，改完宿主自己知道
//! 怎么把结果搬回配置。
//!
//! ## 一条硬规则
//!
//! **每个入口都返回 [`PoolEffect`] 列表**（宿主据此决定要不要重建 provider /
//! 落盘 / 广播）。规则层不直接产生副作用——它只说"改成了什么，需要什么"。

use echo_defs::api_profile::{ApiProfile, ReasoningEffort, ThinkingMode};

/// 池管理能用到的**当前生效默认值**（顶层 `[agent]` 的那几项）。
///
/// 新建 profile 时空字段从这里继承：残缺 profile 单独做 `TestApi` 时必然
/// "provider build failed"，且极易误导用户以为 LLM 整体不可用。
#[derive(Debug, Clone, Default)]
pub struct PoolDefaults {
    /// 顶层 `[agent].provider`。
    pub provider: String,
    /// 顶层 `[agent].model`。
    pub model: String,
    /// 顶层 `[agent].base_url`。
    pub base_url: String,
    /// 顶层 `[agent].api_key`（新建 profile 不继承它——密钥必须显式给）。
    pub api_key: String,
    /// 顶层思考模式。
    pub thinking: ThinkingMode,
    /// 顶层推理强度。
    pub reasoning_effort: ReasoningEffort,
}

impl PoolDefaults {
    /// 顶层默认是否已有可用密钥（只回布尔，绝不回显）。
    pub fn key_set(&self) -> bool {
        !self.api_key.trim().is_empty()
    }
}

/// 池的**借用视图**：只有参与管理规则的那几项。
pub struct PoolState<'a> {
    /// 供应商池（`[agent].api_profiles`）。
    pub profiles: &'a mut Vec<ApiProfile>,
    /// 全局激活的 profile 名（空 = 顶层默认）。
    pub active_api: &'a mut String,
}

/// 一条池编辑**要求的副作用**（由宿主执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolEffect {
    /// 配置变了：重建 provider（名字/端点/密钥都在构建时烘焙）。
    RebuildProvider,
    /// 落盘（`core.toml`）。
    Persist,
    /// 回推 `ApiConfigUpdated` + `ApiProfilesUpdated`（面板刷新）。
    BroadcastApiConfig,
    /// 回推 `TeamsList`（改名影响了 persona 引用）。
    BroadcastTeams,
    /// 给用户的一行回执。
    Notice(String),
}

/// 一次 upsert 的输入（与 `BackendCommand::UpdateApiConfig` 同形）。
#[derive(Debug, Clone, Default)]
pub struct UpsertInput {
    /// 空 = 改**顶层默认**配置；非空 = 指定 profile。
    pub name: String,
    /// 服务商种类（空 = 新建时继承、更新时保留）。
    pub provider: String,
    /// 模型名（同上）。
    pub model: String,
    /// 端点（同上）。
    pub base_url: String,
    /// 空 = 保留原值（"不改密钥"与"清空密钥"不是同一件事）。
    pub api_key: String,
    /// `None` = 不改。
    pub thinking: Option<ThinkingMode>,
    /// `None` = 不改。
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl UpsertInput {
    /// 这条输入实际改到的东西（回执文案用）。
    pub fn target(&self) -> &str {
        if self.name.is_empty() {
            "<default>"
        } else {
            &self.name
        }
    }
}

/// 新建或更新一个 profile（`name` 为空则改顶层默认）。
///
/// 三条规则（逐条对应一次历史修复）：
///
/// 1. **新建时空字段从当前生效配置继承**——否则写出残缺 profile，`TestApi`
///    必然失败且误导用户；
/// 2. **更新时"空 = 保留原值"**——`api_key` 留空表示"不改密钥"，不是清空；
/// 3. **非空 name 立即设为 active**（用户刚配的就是要用的）。
pub fn upsert(state: &mut PoolState<'_>, defaults: &PoolDefaults, input: UpsertInput) -> Vec<PoolEffect> {
    let target = input.target().to_string();
    let keep_key = input.api_key.is_empty();
    if input.name.is_empty() {
        return vec![PoolEffect::Notice(format!(
            "default API {}（detail 由宿主填）",
            if keep_key { "updated" } else { "replaced" }
        ))];
    }
    // 新建（池里没有这个名字）：空字段继承当前生效配置。
    let is_new = !state.profiles.iter().any(|p| p.name == input.name);
    let (provider, model, base_url) = if is_new {
        (
            if input.provider.is_empty() {
                defaults.provider.clone()
            } else {
                input.provider.clone()
            },
            if input.model.is_empty() {
                defaults.model.clone()
            } else {
                input.model.clone()
            },
            if input.base_url.is_empty() {
                defaults.base_url.clone()
            } else {
                input.base_url.clone()
            },
        )
    } else {
        (input.provider.clone(), input.model.clone(), input.base_url.clone())
    };
    let profile = match state.profiles.iter_mut().find(|p| p.name == input.name) {
        Some(profile) => profile,
        None => {
            state
                .profiles
                .push(ApiProfile::new(input.name.clone(), provider.clone(), model.clone()));
            let index = state.profiles.len() - 1;
            &mut state.profiles[index]
        }
    };
    if !provider.is_empty() {
        profile.provider = provider;
    }
    if !model.is_empty() {
        profile.model = model;
    }
    if !base_url.is_empty() {
        profile.base_url = base_url;
    }
    if !keep_key {
        profile.api_key = input.api_key;
    }
    if let Some(thinking) = input.thinking {
        profile.thinking = thinking;
    }
    if let Some(effort) = input.reasoning_effort {
        profile.reasoning_effort = effort;
    }
    *state.active_api = input.name.clone();
    vec![
        PoolEffect::RebuildProvider,
        PoolEffect::Persist,
        PoolEffect::BroadcastApiConfig,
        PoolEffect::Notice(format!("API saved and activated: {target}")),
    ]
}

/// 改**顶层默认**配置（`name` 为空的那条路径）。
///
/// 与 [`upsert`] 分开是因为语义不同：这里不去动 `active_api`——"改默认"与
/// "切到某个 profile"是两件事，清了 active 会让下次请求落到一个用户没选过
/// 的配置上。
pub fn update_defaults(
    defaults: &mut PoolDefaults,
    input: &UpsertInput,
) -> Vec<PoolEffect> {
    let keep_key = input.api_key.is_empty();
    if !input.provider.is_empty() {
        defaults.provider = input.provider.clone();
    }
    if !input.model.is_empty() {
        defaults.model = input.model.clone();
    }
    if !input.base_url.is_empty() {
        defaults.base_url = input.base_url.clone();
    }
    if !keep_key {
        defaults.api_key = input.api_key.clone();
    }
    if let Some(thinking) = input.thinking {
        defaults.thinking = thinking;
    }
    if let Some(effort) = input.reasoning_effort {
        defaults.reasoning_effort = effort;
    }
    vec![
        PoolEffect::RebuildProvider,
        PoolEffect::Persist,
        PoolEffect::BroadcastApiConfig,
        PoolEffect::Notice(format!(
            "default API updated: {} / {}",
            defaults.provider, defaults.model
        )),
    ]
}

/// 切换到某个 profile（空 = 回到顶层默认）。
///
/// 找不到就**明确报错**（不是静默回到默认）——用户点了不存在的配置，回执
/// 必须说清楚。
pub fn switch(state: &mut PoolState<'_>, name: &str) -> Result<Vec<PoolEffect>, String> {
    if name.is_empty() {
        state.active_api.clear();
    } else if state.profiles.iter().any(|p| p.name == name) {
        *state.active_api = name.to_string();
    } else {
        return Err(format!("API config not found: {name}"));
    }
    let label = if name.is_empty() {
        "default config".to_string()
    } else {
        name.to_string()
    };
    Ok(vec![
        PoolEffect::RebuildProvider,
        PoolEffect::Persist,
        PoolEffect::BroadcastApiConfig,
        PoolEffect::Notice(format!("switched to API: {label}")),
    ])
}

/// 删除一个 profile：若删的是激活项，`active_api` 清空（回落到顶层默认）。
///
/// 返回 `Ok(None)` = 池里本来就没有（宿主据此回"not found"，且**不**落盘、
/// **不**重建 provider——零写入）。
pub fn delete(state: &mut PoolState<'_>, name: &str) -> Vec<PoolEffect> {
    let before = state.profiles.len();
    state.profiles.retain(|profile| profile.name != name);
    if state.profiles.len() == before {
        return vec![PoolEffect::Notice(format!(
            "API config not found: {name}"
        ))];
    }
    if *state.active_api == name {
        state.active_api.clear();
    }
    vec![
        PoolEffect::RebuildProvider,
        PoolEffect::Persist,
        PoolEffect::BroadcastApiConfig,
        PoolEffect::Notice(format!("deleted API config: {name}")),
    ]
}

/// 改名计划（在克隆上算好，宿主落盘成功后才提交）。
#[derive(Debug, Clone)]
pub struct RenamePlan {
    /// 改后的 profile 列表（调用方直接替换）。
    pub profiles: Vec<ApiProfile>,
    /// 改后的 `active_api`。
    pub active_api: String,
    /// 是否影响了激活项（宿主据此判断要不要重绑会话覆盖）。
    pub active_changed: bool,
}

/// 校验新名字：非空、≤128 字符、无控制字符。
///
/// 为什么在插件里：它是**池的命名规则**，与"面板怎么展示"无关。宿主只负责
/// 把错误变成回执。
pub fn validate_profile_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("API rename failed: new name is empty".into());
    }
    if trimmed.chars().count() > 128 || trimmed.chars().any(char::is_control) {
        return Err(
            "API rename failed: new name must be ≤128 chars without control characters".into(),
        );
    }
    Ok(())
}

/// 计算改名后的池状态（**不改入参**）。
///
/// 一致性要求（2026-10）：先在克隆上算全新状态 → 宿主单次落盘（失败即中止，
/// 内存从未提交 = 天然回滚）→ 提交内存 → 运行时传播。所以这里只**算**，
/// 不提交。
pub fn plan_rename(
    state: &PoolState<'_>,
    from: &str,
    to: &str,
) -> Result<RenamePlan, String> {
    validate_profile_name(to)?;
    let to = to.trim();
    // `from` 精确匹配（不 trim——历史遗留含空白的名也要能被改名）。
    if from.is_empty() || !state.profiles.iter().any(|profile| profile.name == from) {
        return Err(format!("API config not found: {from}"));
    }
    if from == to {
        return Ok(RenamePlan {
            profiles: state.profiles.clone(),
            active_api: state.active_api.clone(),
            active_changed: false,
        });
    }
    if state.profiles.iter().any(|profile| profile.name == to) {
        return Err(format!("API rename failed: {to} already exists"));
    }
    let mut profiles = state.profiles.clone();
    for profile in profiles.iter_mut() {
        if profile.name == from {
            profile.name = to.to_string();
        }
    }
    let active_changed = state.active_api == from;
    let active_api = if active_changed {
        to.to_string()
    } else {
        state.active_api.clone()
    };
    Ok(RenamePlan {
        profiles,
        active_api,
        active_changed,
    })
}

/// 改名生效后的副作用清单（宿主在落盘成功后执行）。
pub fn rename_effects(from: &str, to: &str, active_changed: bool, teams_changed: bool) -> Vec<PoolEffect> {
    let mut effects = vec![
        PoolEffect::RebuildProvider,
        PoolEffect::BroadcastApiConfig,
        PoolEffect::Notice(format!("renamed API config: {from} → {to}")),
    ];
    if active_changed {
        // 激活项改名：provider 名字在构建时烘焙，必须重建（已在列表里）。
        // 这里只补"重绑会话覆盖"的语义标记——宿主把它实现为
        // `rebind_session_providers`。
        effects.push(PoolEffect::Notice(format!(
            "active API renamed: session overrides rebound"
        )));
    }
    if teams_changed {
        effects.push(PoolEffect::BroadcastTeams);
    }
    effects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> PoolDefaults {
        PoolDefaults {
            provider: "deepseek".into(),
            model: "deepseek-flash".into(),
            base_url: "https://api.deepseek.com".into(),
            api_key: "sk-default".into(),
            ..Default::default()
        }
    }

    fn pool(profiles: Vec<ApiProfile>, active: &str) -> (Vec<ApiProfile>, String) {
        (profiles, active.to_string())
    }

    fn state<'a>(profiles: &'a mut Vec<ApiProfile>, active: &'a mut String) -> PoolState<'a> {
        PoolState {
            profiles,
            active_api: active,
        }
    }

    /// 新建 profile 时空字段**从当前生效配置继承**（防残缺 profile）。
    #[test]
    fn new_profile_inherits_defaults_for_empty_fields() {
        let (mut profiles, mut active) = pool(vec![], "");
        let effects = upsert(
            &mut state(&mut profiles, &mut active),
            &defaults(),
            UpsertInput {
                name: "work".into(),
                ..Default::default()
            },
        );
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].provider, "deepseek", "空 provider 要继承");
        assert_eq!(profiles[0].model, "deepseek-flash", "空 model 要继承");
        assert_eq!(profiles[0].base_url, "https://api.deepseek.com");
        assert_eq!(active, "work", "新建后立即激活");
        assert!(effects.contains(&PoolEffect::RebuildProvider));
        assert!(effects.contains(&PoolEffect::Persist));
    }

    /// 更新时不继承（空字段 = 保留该 profile 原值）。
    #[test]
    fn updating_keeps_existing_values_and_empty_key_preserves_secret() {
        let mut existing = ApiProfile::new("work", "openai", "gpt-x");
        existing.api_key = "sk-old".into();
        let (mut profiles, mut active) = pool(vec![existing], "work");
        upsert(
            &mut state(&mut profiles, &mut active),
            &defaults(),
            UpsertInput {
                name: "work".into(),
                model: "gpt-y".into(),
                api_key: String::new(),
                ..Default::default()
            },
        );
        assert_eq!(profiles[0].provider, "openai", "空 provider 不该被默认值覆盖");
        assert_eq!(profiles[0].model, "gpt-y");
        assert_eq!(profiles[0].api_key, "sk-old", "空 key = 不改密钥");
    }

    /// 非空 key 会替换（"清空密钥"只能靠显式删除 profile）。
    #[test]
    fn non_empty_key_replaces_the_secret() {
        let mut existing = ApiProfile::new("work", "openai", "gpt-x");
        existing.api_key = "sk-old".into();
        let (mut profiles, mut active) = pool(vec![existing], "work");
        upsert(
            &mut state(&mut profiles, &mut active),
            &defaults(),
            UpsertInput {
                name: "work".into(),
                api_key: "sk-new".into(),
                ..Default::default()
            },
        );
        assert_eq!(profiles[0].api_key, "sk-new");
    }

    /// 切到不存在的 profile：**明确报错**（不是静默回默认）。
    #[test]
    fn switching_to_unknown_profile_is_an_error() {
        let (mut profiles, mut active) = pool(vec![], "");
        let error = switch(&mut state(&mut profiles, &mut active), "ghost").unwrap_err();
        assert!(error.contains("not found"), "{error}");
    }

    /// 空名 = 回顶层默认（清空 active）。
    #[test]
    fn switching_to_empty_clears_active() {
        let (mut profiles, mut active) = pool(vec![ApiProfile::new("work", "p", "m")], "work");
        switch(&mut state(&mut profiles, &mut active), "").expect("回到默认");
        assert!(active.is_empty());
    }

    /// 删激活项：active 清空（回落顶层默认）。
    #[test]
    fn deleting_the_active_profile_clears_active() {
        let (mut profiles, mut active) = pool(vec![ApiProfile::new("work", "p", "m")], "work");
        delete(&mut state(&mut profiles, &mut active), "work");
        assert!(profiles.is_empty());
        assert!(active.is_empty(), "删掉激活项必须回落默认");
    }

    /// 删不存在的：只回回执，**不**落盘、**不**重建 provider（零写入）。
    #[test]
    fn deleting_an_unknown_profile_has_no_side_effects() {
        let (mut profiles, mut active) = pool(vec![], "");
        let effects = delete(&mut state(&mut profiles, &mut active), "ghost");
        assert_eq!(
            effects,
            vec![PoolEffect::Notice("API config not found: ghost".into())]
        );
    }

    /// 改名：连带改 active，且不改入参（克隆上算）。
    #[test]
    fn plan_rename_rewrites_the_active_pointer_on_a_clone() {
        let (mut profiles, mut active) = pool(vec![ApiProfile::new("old", "p", "m")], "old");
        let plan = plan_rename(&state(&mut profiles, &mut active), "old", "new").expect("可改名");
        assert_eq!(plan.profiles[0].name, "new");
        assert_eq!(plan.active_api, "new");
        assert!(plan.active_changed);
        // 入参未被改（落盘成功后才提交）。
        assert_eq!(profiles[0].name, "old");
        assert_eq!(active, "old");
    }

    /// 改名冲突 / 源不存在 / 非法名字：三种都明确报错。
    #[test]
    fn plan_rename_rejects_conflicts_and_bad_names() {
        let (mut profiles, mut active) = pool(
            vec![
                ApiProfile::new("a", "p", "m"),
                ApiProfile::new("b", "p", "m"),
            ],
            "a",
        );
        let st = state(&mut profiles, &mut active);
        assert!(plan_rename(&st, "a", "b").unwrap_err().contains("already exists"));
        assert!(plan_rename(&st, "ghost", "c").unwrap_err().contains("not found"));
        assert!(plan_rename(&st, "a", "").is_err(), "空名必须被拒");
        let long = "x".repeat(129);
        assert!(plan_rename(&st, "a", &long).is_err(), "超长名必须被拒");
    }

    /// 名字里的控制字符被拒（防日志/面板注入）。
    #[test]
    fn validate_name_rejects_control_characters() {
        assert!(validate_profile_name("ok-name").is_ok());
        assert!(validate_profile_name("bad\nname").is_err());
    }
}
