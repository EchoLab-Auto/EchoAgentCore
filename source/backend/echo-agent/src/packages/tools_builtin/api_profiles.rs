//! `list_api_profiles` 工具：列出可用的模型供应商（API profiles）。
//!
//! 面向模型：委派子代理（`spawn_subagent` 的 `profile` 参数）或需要了解
//! "有哪些模型可用"时先查这里——返回名称 / 服务商 / 模型 / 端点 / 密钥
//! 就绪状态；模型选择规范见 `model-profiles` 技能。
//!
//! 数据源：共享 `ConfigStore`（core.toml 的 `[agent]`——全局供应商池与管理面
//! 顶层默认的持久化事实源；见 [`crate::api_pool`] 模块文档）。输出**绝不含
//! 明文密钥**，仅布尔"是否已配置"。

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::api_pool::PoolSnapshot;
use crate::tool::{Tool, ToolError};

pub struct ListApiProfilesTool {
    store: echo_adapter::ConfigStore,
}

impl ListApiProfilesTool {
    pub fn new(store: echo_adapter::ConfigStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for ListApiProfilesTool {
    fn name(&self) -> &str {
        "list_api_profiles"
    }

    fn description(&self) -> &str {
        "列出可用的模型供应商（API profile）：名称 / 服务商 / 模型 / 端点 / API Key 就绪状态。\
         用于为 spawn_subagent 的 `profile` 参数挑选目标模型（模型选择规范见 model-profiles 技能）。无参数。"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
        let snapshot = crate::api_pool::read_snapshot(&self.store).map_err(ToolError::Execution)?;
        Ok(render(&snapshot))
    }
}

/// 空值显示为「（继承默认）」——profile 的空字段在解析时回退上一级配置。
fn dash(value: &str) -> &str {
    if value.trim().is_empty() {
        "（继承默认）"
    } else {
        value
    }
}

fn render(snapshot: &PoolSnapshot) -> String {
    let mut out = String::new();
    if snapshot.profiles.is_empty() {
        out.push_str("当前未配置任何 API profile（供应商池为空）——可在 Panel「API 设置」添加。\n");
    } else {
        out.push_str(&format!(
            "可用模型供应商（API profile 共 {} 个）：\n",
            snapshot.profiles.len()
        ));
        for (i, p) in snapshot.profiles.iter().enumerate() {
            let active = if !snapshot.active_api.is_empty() && snapshot.active_api == p.name {
                "（全局激活）"
            } else {
                ""
            };
            out.push_str(&format!(
                "{}. {}{} — provider={}, model={}, 思考={}, 推理={}；API Key {}；端点 {}\n",
                i + 1,
                p.name,
                active,
                dash(&p.provider),
                dash(&p.model),
                p.thinking.as_str(),
                p.reasoning_effort.as_str(),
                if p.api_key.trim().is_empty() {
                    "⚠ 未配置"
                } else {
                    "✓ 已配置"
                },
                dash(&p.base_url),
            ));
        }
    }
    out.push_str(&format!(
        "\n顶层默认（未选 profile 时生效）：provider={}, model={}, API Key {}\n",
        dash(&snapshot.default_provider),
        dash(&snapshot.default_model),
        if snapshot.default_key_set {
            "✓ 已配置"
        } else {
            "⚠ 未配置"
        },
    ));
    out.push_str(
        "用法：spawn_subagent 的 `profile` 参数可填上面任一名称（仅影响该子任务，不改变主 agent）；\
         不填 = 继承当前 agent 模型。模型选择规范见 model-profiles 技能。\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(content: &str) -> String {
        let path = std::env::temp_dir().join(format!(
            "echo-list-profiles-{}-{}.toml",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, content).unwrap();
        let tool = ListApiProfilesTool::new(echo_adapter::ConfigStore::new(path.clone()));
        let result = tool.execute(json!({})).await.expect("execute");
        let _ = std::fs::remove_file(&path);
        result
    }

    #[tokio::test]
    async fn lists_profiles_flags_and_defaults_without_secrets() {
        let out = run(
            "[agent]\nprovider = \"deepseek\"\nmodel = \"deepseek-flash\"\napi_key = \"sk-top-secret\"\nactive_api = \"deepseek\"\n\n\
             [[agent.api_profiles]]\nname = \"deepseek\"\nprovider = \"deepseek\"\nmodel = \"deepseek-flash\"\napi_key = \"sk-a-secret\"\n\n\
             [[agent.api_profiles]]\nname = \"kimi\"\nprovider = \"kimi\"\nmodel = \"k3\"\nbase_url = \"https://api.kimi.com/coding\"\n",
        )
        .await;
        assert!(out.contains("共 2 个"), "{out}");
        assert!(out.contains("deepseek（全局激活）"), "{out}");
        assert!(out.contains("kimi"), "{out}");
        assert!(out.contains("model=k3"), "{out}");
        assert!(out.contains("https://api.kimi.com/coding"), "{out}");
        // kimi 未配 key → 告警
        assert!(out.contains("⚠ 未配置"), "{out}");
        assert!(out.contains("顶层默认"), "{out}");
        assert!(out.contains("spawn_subagent"), "{out}");
        // 绝不回显密钥
        assert!(!out.contains("sk-top-secret"), "{out}");
        assert!(!out.contains("sk-a-secret"), "{out}");
    }

    #[tokio::test]
    async fn empty_pool_is_reported_with_guidance() {
        let out = run("[agent]\nprovider = \"deepseek\"\nmodel = \"deepseek-flash\"\n").await;
        assert!(out.contains("未配置任何 API profile"), "{out}");
        assert!(out.contains("Panel「API 设置」"), "{out}");
    }

    #[tokio::test]
    async fn unreadable_config_reports_error() {
        let dir = std::env::temp_dir().join(format!(
            "echo-list-profiles-bad-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("core.toml");
        std::fs::write(&path, "not = valid = toml\n").unwrap();
        let tool = ListApiProfilesTool::new(echo_adapter::ConfigStore::new(path));
        assert!(tool.execute(json!({})).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
