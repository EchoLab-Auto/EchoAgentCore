//! API 设置面的**策略与词汇**（原先在宿主 `agent/api_admin.rs`）。
//!
//! ## 为什么搬到这里
//!
//! 这个文件此前 1169 行住在 `echo-agent/src/agent/api_admin.rs`，混合了三类东西：
//!
//! | 类别 | 例子 | 2026-11 之后的归属 |
//! |---|---|---|
//! | **纯逻辑**（无宿主状态） | 探针配置解析、指标目标推导、`core.toml` 的 agent 表补丁、`ApiConfigUpdated` 事件装配 | **本模块** |
//! | **策略**（池规则） | 空字段继承 / 空 key 保留 / 改名连带 | 已在 `admin.rs`（更早搬过） |
//! | **状态副作用** | `Agent.model.config` 的锁、provider 重建、落盘、余额快照落库 | 宿主（[`ApiAdminHost`] 接缝） |
//!
//! 三类混在一个 crate 的后果是：改一句回执文案要重编宿主，而宿主又带着
//! turn 循环、工具派发、模块宿主——一次文案改动触发全量重编。
//!
//! ## 口径一致性
//!
//! [`ApiSnapshot`] 复制了宿主 `AgentConfig` 的四个解析口径
//! （`active_profile` / `apply_active_profile` / `effective_api_key` /
//! `effective_base_url`）。**复制是有意的**：让模块不依赖宿主的配置类型，
//! 代价是两份实现可能漂移——由 `echo-agent` 侧的
//! `api_snapshot_matches_agent_config` 测试守着（同输入比结果）。

use echo_defs::api_profile::ApiProfile;
use echo_defs::mode::{ReasoningEffort, ThinkingMode};
use echo_module_management::protocol::event::{ApiProfileInfo, BackendEvent};

/// API 相关配置的**窄视图**：只带设置面需要的字段。
///
/// 刻意不含 `max_tool_iterations` / `skills_dir` / `teams` 这类与 API 无关
/// 的宿主配置——模块认识它们就等于认识宿主。
#[derive(Debug, Clone, Default)]
pub struct ApiSnapshot {
    /// 顶层默认 provider（`active_api` 为空时生效）。
    pub provider: String,
    /// 顶层默认模型名。
    pub model: String,
    /// 顶层默认端点（空 = 按 provider 取缺省）。
    pub base_url: String,
    /// 顶层默认密钥（空 = 读环境变量）。
    pub api_key: String,
    /// DeepSeek 思考模式开关。
    pub thinking: ThinkingMode,
    /// 推理强度（`max` 为默认）。
    pub reasoning_effort: ReasoningEffort,
    /// 命名 profile 池。
    pub api_profiles: Vec<ApiProfile>,
    /// 当前激活的 profile 名；空 = 用顶层字段。
    pub active_api: String,
    /// 系统提示词（`ApiConfigUpdated` 带它，面板据此显示）。
    pub system_prompt: String,
}

impl ApiSnapshot {
    /// 激活的 profile（`active_api` 为空或找不到 = `None`）。
    pub fn active_profile(&self) -> Option<&ApiProfile> {
        if self.active_api.is_empty() {
            return None;
        }
        self.api_profiles.iter().find(|p| p.name == self.active_api)
    }

    /// 把激活 profile 的非空值合入顶层字段（与 `AgentConfig` 同口径）。
    pub fn apply_active_profile(&mut self) {
        let Some(profile) = self.active_profile().cloned() else {
            return;
        };
        overlay(&mut self.provider, profile.provider);
        overlay(&mut self.model, profile.model);
        overlay(&mut self.base_url, profile.base_url);
        overlay(&mut self.api_key, profile.api_key);
        self.thinking = profile.thinking;
        self.reasoning_effort = profile.reasoning_effort;
    }

    /// 生效的 API key：显式值优先，缺失时按 provider 读环境变量。
    pub fn effective_api_key(&self) -> String {
        if !self.api_key.is_empty() {
            return self.api_key.clone();
        }
        let env_var = match self.provider.as_str() {
            "anthropic" | "claude" => "ANTHROPIC_API_KEY",
            "deepseek" => "DEEPSEEK_API_KEY",
            "ollama" => "",
            _ => "OPENAI_API_KEY",
        };
        if env_var.is_empty() {
            String::new()
        } else {
            std::env::var(env_var).unwrap_or_default()
        }
    }

    /// 生效的 base_url：显式值优先，缺失时按 provider 取缺省端点。
    pub fn effective_base_url(&self) -> String {
        if !self.base_url.is_empty() {
            return self.base_url.clone();
        }
        match self.provider.as_str() {
            "anthropic" | "claude" => "https://api.anthropic.com".into(),
            "deepseek" => "https://api.deepseek.com".into(),
            "ollama" => "http://localhost:11434".into(),
            _ => "https://api.openai.com/v1".into(),
        }
    }
}

/// 非空覆盖（与 `AgentConfig::apply_active_profile` 的宽松语义一致）。
fn overlay(field: &mut String, value: String) {
    if !value.is_empty() {
        *field = value;
    }
}

/// 构造测连通性用的配置快照。
///
/// `name` 空 → 顶层配置合入激活 profile；非空 → 该 profile 的非空字段覆盖
/// 当前生效配置（字段残缺的 profile 也能测出它本来的行为）。profile 不存在
/// 或解析后没有 provider 时返回可读错误。
pub fn resolve_probe_config(snapshot: &ApiSnapshot, name: &str) -> Result<ApiSnapshot, String> {
    let mut probe = snapshot.clone();
    if name.is_empty() {
        probe.apply_active_profile();
    } else {
        let profile = snapshot
            .api_profiles
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("profile not found: {name}"))?;
        // 与 `apply_active_profile` 一致的宽松语义：profile 空字段回退到
        // 当前生效值——残缺 profile 单独测不出本来配置会误导用户。
        overlay(&mut probe.provider, profile.provider.clone());
        overlay(&mut probe.model, profile.model.clone());
        overlay(&mut probe.base_url, profile.base_url.clone());
        overlay(&mut probe.api_key, profile.api_key.clone());
        probe.thinking = profile.thinking;
        probe.reasoning_effort = profile.reasoning_effort;
    }
    if probe.provider.is_empty() {
        return Err(format!("profile {name} has no provider"));
    }
    Ok(probe)
}

/// 余额/指标查询的**目标列表**：顶层默认（`""`）+ 各 profile 名。
///
/// 顶层默认与 `active_api` 指向同一配置时（空名解析会合入该 profile），
/// 去重跳过空名，避免同一账户产生双份序列。
pub fn metrics_targets(snapshot: &ApiSnapshot) -> Vec<String> {
    let active = snapshot.active_api.trim();
    let mut targets = Vec::with_capacity(snapshot.api_profiles.len() + 1);
    if active.is_empty() || !snapshot.api_profiles.iter().any(|p| p.name == active) {
        targets.push(String::new());
    }
    targets.extend(snapshot.api_profiles.iter().map(|p| p.name.clone()));
    targets
}

/// 把 `agent_value`（宿主序列化好的 `[agent]` 表）装进 `root`，并保住两条口径：
///
/// - `api_key` 排除在泛型序列化之外：显式键非空时写入该值，否则**保留磁盘
///   现值**——任何无关保存都不得把文件里的 key 抹掉；
/// - `teams` / `disabled_teams` 由人格管理器独立持久化：快照里的副本可能已
///   过期，一律以磁盘为准，避免"某次无关保存把人格名单写回旧值"。
///
/// 参数是**已经序列化好的表**而不是宿主的配置类型：补丁规则是本模块的知识，
/// 而"怎么序列化"是宿主的（`#[serde(skip_serializing)]` 之类）。
pub fn patch_agent_table(
    root: &mut toml::Table,
    mut agent_value: toml::Value,
    api_key: &str,
) -> Result<(), String> {
    let key_value = if api_key.is_empty() {
        root.get("agent")
            .and_then(|agent| agent.get("api_key"))
            .cloned()
    } else {
        Some(toml::Value::String(api_key.to_string()))
    };
    let table = agent_value
        .as_table_mut()
        .ok_or_else(|| "serialized agent config is not a table".to_string())?;
    if let Some(key_value) = key_value {
        table.insert("api_key".into(), key_value);
    }
    for key in ["teams", "disabled_teams"] {
        let disk = root.get("agent").and_then(|agent| agent.get(key)).cloned();
        match disk {
            Some(value) => {
                table.insert(key.into(), value);
            }
            None => {
                table.remove(key);
            }
        }
    }
    root.insert("agent".into(), agent_value);
    Ok(())
}

/// `ApiConfigUpdated` / `ApiProfilesUpdated` 两条事件（面板重连的全量快照用）。
///
/// 参数 `model` 是**生效模型名**（宿主解析：可能是会话级覆盖），模块不猜。
pub fn api_config_events(snapshot: &ApiSnapshot, model: &str) -> Vec<BackendEvent> {
    let mut resolved = snapshot.clone();
    resolved.apply_active_profile();
    let profiles: Vec<ApiProfileInfo> = snapshot
        .api_profiles
        .iter()
        .map(|p| ApiProfileInfo {
            name: p.name.clone(),
            provider: p.provider.clone(),
            model: p.model.clone(),
            base_url: p.base_url.clone(),
            api_key_set: !p.api_key.is_empty(),
            thinking: p.thinking,
            reasoning_effort: p.reasoning_effort,
        })
        .collect();
    vec![
        BackendEvent::ApiConfigUpdated {
            provider: resolved.provider.clone(),
            model: model.to_string(),
            base_url: resolved.effective_base_url(),
            api_key_set: !resolved.effective_api_key().is_empty(),
            thinking: resolved.thinking,
            reasoning_effort: resolved.reasoning_effort,
            system_prompt: snapshot.system_prompt.clone(),
            active_api: snapshot.active_api.clone(),
            profiles: profiles.clone(),
        },
        BackendEvent::ApiProfilesUpdated {
            active_api: snapshot.active_api.clone(),
            profiles,
        },
    ]
}

/// 探针结果 → `(ok, message)`（面板文案）。
///
/// `content` 是模型回复正文；`Err` 是请求失败原因（已含超时）。
pub fn probe_result(model: &str, outcome: Result<String, String>) -> (bool, String) {
    match outcome {
        Ok(content) => {
            let content = content.trim().to_string();
            (
                true,
                if content.is_empty() {
                    format!("OK ({model}) — 收到空响应")
                } else {
                    format!(
                        "OK ({model}) — 响应: {}",
                        echo_defs::token::truncate(&content, 60)
                    )
                },
            )
        }
        Err(error) => (false, format!("请求失败: {error}")),
    }
}

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// 宿主接缝：只有宿主做得了的那些事。
///
/// 与 `commands::ProviderCommandHost` 的分工：那个管**写命令**（改配置、切池），
/// 这个管**读快照 + 副作用出口**（重建 provider / 落盘 / 探针 / 事件）。
pub trait ApiAdminHost: Send + Sync {
    /// 读一次配置快照。
    fn snapshot(&self) -> BoxFuture<'_, ApiSnapshot>;
    /// 用快照重建 provider；`true` = 成功（宿主随后把模型名同步到运行时）。
    fn rebuild_provider<'a>(&'a self, snapshot: ApiSnapshot) -> BoxFuture<'a, bool>;
    /// 落盘（`core.toml` 的 `[agent]` 表）。
    fn persist<'a>(&'a self, snapshot: ApiSnapshot) -> BoxFuture<'a, ()>;
    /// 一次探针往返（宿主建 provider、发请求、返回正文）。
    ///
    /// `Err` = 请求失败或超时（文案已本地化）；宿主自己决定用哪个端点。
    fn probe<'a>(&'a self, snapshot: ApiSnapshot) -> BoxFuture<'a, Result<String, String>>;
    /// 发一条线协议事件。
    fn emit(&self, event: BackendEvent);
}

/// 「测试 API」命令的完整流程（对齐 `test_api_config` 的语义与事件）。
///
/// 顺序是刻意的：先解析探针配置 → 失败就发错误结果并**返回**（不重建
/// provider）；成功才探针。整个流程只有**一次**读快照——中途配置变更不会
/// 让"测的"和"报的"变成两份不同的东西。
pub async fn run_test_api(host: &dyn ApiAdminHost, name: &str) {
    let started = std::time::Instant::now();
    let snapshot = host.snapshot().await;
    let probe = match resolve_probe_config(&snapshot, name) {
        Ok(probe) => probe,
        Err(message) => {
            host.emit(BackendEvent::ApiTestResult {
                name: name.into(),
                ok: false,
                message,
                latency_ms: started.elapsed().as_millis() as u64,
            });
            return;
        }
    };
    let model = probe.model.clone();
    let (ok, message) = probe_result(&model, host.probe(probe).await);
    host.emit(BackendEvent::ApiTestResult {
        name: name.into(),
        ok,
        message,
        latency_ms: started.elapsed().as_millis() as u64,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ApiSnapshot {
        ApiSnapshot {
            provider: "openai".into(),
            model: "top".into(),
            base_url: String::new(),
            api_key: "k".into(),
            thinking: ThinkingMode::Disabled,
            reasoning_effort: ReasoningEffort::Low,
            api_profiles: vec![ApiProfile {
                name: "p1".into(),
                provider: "deepseek".into(),
                model: "m1".into(),
                ..Default::default()
            }],
            active_api: String::new(),
            system_prompt: "sys".into(),
        }
    }

    #[test]
    fn active_profile_overlays_only_nonempty_fields() {
        let mut snapshot = snapshot();
        snapshot.active_api = "p1".into();
        snapshot.apply_active_profile();
        assert_eq!(snapshot.provider, "deepseek");
        assert_eq!(snapshot.model, "m1");
        // profile 未设 base_url → 保留顶层（空串 → 走 provider 缺省）。
        assert_eq!(snapshot.base_url, "");
        assert_eq!(snapshot.effective_base_url(), "https://api.deepseek.com");
    }

    #[test]
    fn probe_config_prefers_named_profile_then_falls_back() {
        let snapshot = snapshot();
        let probe = resolve_probe_config(&snapshot, "p1").expect("named");
        assert_eq!(probe.model, "m1");
        // 其余字段回退到当前生效值。
        assert_eq!(probe.api_key, "k");
        assert!(resolve_probe_config(&snapshot, "nope").is_err());
    }

    #[test]
    fn probe_config_rejects_providerless_result() {
        let mut snapshot = snapshot();
        snapshot.provider.clear();
        snapshot.model.clear();
        assert!(resolve_probe_config(&snapshot, "").is_err());
    }

    /// 目标是"顶层默认 + 各 profile"；顶层默认与激活 profile 重合时去重。
    #[test]
    fn metrics_targets_dedupes_the_active_profile() {
        let snapshot = snapshot();
        assert_eq!(
            metrics_targets(&snapshot),
            vec!["".to_string(), "p1".into()]
        );
        let mut active = snapshot.clone();
        active.active_api = "p1".into();
        assert_eq!(metrics_targets(&active), vec!["p1".to_string()]);
    }

    /// `api_key` 的两种来源：显式写 vs 保留磁盘现值。
    #[test]
    fn patch_keeps_disk_key_when_snapshot_has_none() {
        let mut root: toml::Table = toml::from_str("[agent]\napi_key = \"disk\"\n").unwrap();
        let mut agent = toml::Table::new();
        agent.insert("model".into(), toml::Value::String("m".into()));
        patch_agent_table(&mut root, toml::Value::Table(agent), "").unwrap();
        assert_eq!(
            root["agent"]["api_key"].as_str(),
            Some("disk"),
            "空 api_key 不得抹掉磁盘值"
        );
        assert_eq!(root["agent"]["model"].as_str(), Some("m"));
    }

    #[test]
    fn patch_writes_explicit_key_and_drops_stale_teams() {
        let mut root: toml::Table =
            toml::from_str("[agent]\napi_key = \"disk\"\n[agent.teams.x]\nname = \"x\"\n").unwrap();
        let mut agent = toml::Table::new();
        agent.insert("teams".into(), toml::Value::Table(Default::default()));
        patch_agent_table(&mut root, toml::Value::Table(agent), "new").unwrap();
        assert_eq!(root["agent"]["api_key"].as_str(), Some("new"));
        // 磁盘有 teams 而快照没有 → 取磁盘值（上一行 root 里有 teams.x）。
        assert!(root["agent"].get("teams").is_some());
    }

    #[test]
    fn probe_result_shapes_empty_and_error() {
        let (ok, message) = probe_result("m", Ok("   ".into()));
        assert!(ok);
        assert!(message.contains("空响应"));
        let (ok, message) = probe_result("m", Err("boom".into()));
        assert!(!ok);
        assert!(message.contains("请求失败"));
    }

    /// 事件装配：`ApiConfigUpdated` 报的是**生效值**（激活 profile 已合入），
    /// `ApiProfilesUpdated` 报的是**池原样**（面板按它渲染下拉）。
    #[test]
    fn api_config_events_report_effective_and_raw() {
        let mut snapshot = snapshot();
        snapshot.active_api = "p1".into();
        let events = api_config_events(&snapshot, "eff-model");
        match &events[0] {
            BackendEvent::ApiConfigUpdated {
                provider,
                model,
                active_api,
                ..
            } => {
                assert_eq!(provider, "deepseek");
                assert_eq!(model, "eff-model");
                assert_eq!(active_api, "p1");
            }
            other => panic!("期望 ApiConfigUpdated，得到 {other:?}"),
        }
        match &events[1] {
            BackendEvent::ApiProfilesUpdated { profiles, .. } => {
                assert_eq!(profiles.len(), 1);
                assert_eq!(profiles[0].provider, "deepseek");
            }
            other => panic!("期望 ApiProfilesUpdated，得到 {other:?}"),
        }
    }
}
