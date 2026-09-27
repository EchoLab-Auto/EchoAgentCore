//! Plugin manifest — the declarative identity of a plugin.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The category of a plugin. Determines where it mounts and its reload mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// Data plugin: SKILL.md-style directory, hot-reloads in seconds.
    Skill,
    /// Tool plugin: registers Tool instances, hot-reloads (reversible).
    Tool,
    /// LLM provider factory (deepseek/openai/anthropic/ollama).
    Provider,
    /// Turn runner / agent loop driver.
    Loop,
    /// Platform adapter (qq, ...).
    Adapter,
    /// Built-in orchestration（保留类型；原 run_sudo/framework_update 工具已于
    /// 2026-09 废弃，当前无内置成员）。
    Orchestration,
    /// Management surface (Panel bridge).
    Management,
    /// Interaction surface: human-in-the-loop UI driven from the agent
    /// （保留类型；原选单（menu）已于 2026-09 废弃，当前无内置成员）。
    Interaction,
}

impl PluginKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PluginKind::Skill => "skill",
            PluginKind::Tool => "tool",
            PluginKind::Provider => "provider",
            PluginKind::Loop => "loop",
            PluginKind::Adapter => "adapter",
            PluginKind::Orchestration => "orchestration",
            PluginKind::Management => "management",
            PluginKind::Interaction => "interaction",
        }
    }
}

/// Declarative manifest for a plugin.
///
/// `id` is the stable key (e.g. `echo-agent.tools.builtin`); `entry` names the
/// mount entry point within the builtin registry (empty for external manifests
/// that carry their own SKILL.md/code).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Stable unique id: `{vendor}.{group}.{name}`.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Which seam this plugin mounts into.
    pub kind: PluginKind,
    /// Builtin mount entry name (composition root resolves it); empty for
    /// data plugins discovered from a plugin directory.
    #[serde(default)]
    pub entry: String,
    /// Human-facing metadata shown in the Panel.
    #[serde(default)]
    pub author: String,
    /// Owning **package** id — the cross-cutting tag spanning
    /// plugin + tools + skills（见 [`Self::package_id`]）。
    /// `None` = 本插件自身即包根：插件 id 同时是包标签（当前全部内置
    /// 插件如此，因此默认行为与历史一致）。声明后，本插件的启停门控
    /// 应以包 id 传播到同名包的工具与技能。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
}

impl PluginManifest {
    /// 本插件所属的**包** id：显式声明优先，未声明时回退为插件 id。
    ///
    /// Package 是横跨三类的标签：插件（本 manifest）声明归属；工具经
    /// `ToolRegistry::set_package` 打标；技能经 `SKILL.md` frontmatter
    /// `package:` 打标。包级门控（禁用一个包 = 其全部成员不可见）即按
    /// 该字符串聚合（QQ 包 = `echo-agent.adapter.qq`）。
    pub fn package_id(&self) -> &str {
        self.package.as_deref().unwrap_or(&self.id)
    }

    /// 显式声明所属包（builder 风格；未调用时包 id = 插件 id）。
    pub fn with_package(mut self, package: impl Into<String>) -> Self {
        self.package = Some(package.into());
        self
    }

    pub fn builtin(
        id: impl Into<String>,
        name: impl Into<String>,
        version: impl Into<String>,
        kind: PluginKind,
        entry: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            version: version.into(),
            description: description.into(),
            kind,
            entry: entry.into(),
            package: None,
            author: "EchoAgentCore".into(),
        }
    }
}

/// Errors from manifest parsing/validation.
#[derive(Debug, Error)]
pub enum PluginError {
    #[error("plugin manifest parse error: {0}")]
    Parse(String),
    #[error("plugin manifest missing required field: {0}")]
    MissingField(String),
    #[error("plugin id conflict: {0}")]
    IdConflict(String),
}

/// Parse a `plugin.toml` manifest from text.
pub fn parse_manifest(text: &str) -> Result<PluginManifest, PluginError> {
    let manifest: PluginManifest =
        toml::from_str(text).map_err(|e| PluginError::Parse(e.to_string()))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Parse a manifest file on disk.
pub fn load_manifest(path: &std::path::Path) -> Result<PluginManifest, PluginError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| PluginError::Parse(format!("{}: {e}", path.display())))?;
    parse_manifest(&text)
}

fn validate_manifest(manifest: &PluginManifest) -> Result<(), PluginError> {
    if manifest.id.trim().is_empty() {
        return Err(PluginError::MissingField("id".into()));
    }
    if manifest.name.trim().is_empty() {
        return Err(PluginError::MissingField("name".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest_toml() {
        let text = r#"
id = "echo-agent.tools.builtin"
name = "内置工具集"
version = "0.1.0"
description = "平台无关的内置工具"
kind = "tool"
entry = "builtin_tools"
author = "EchoAgentCore"
"#;
        let m = parse_manifest(text).unwrap();
        assert_eq!(m.id, "echo-agent.tools.builtin");
        assert_eq!(m.kind, PluginKind::Tool);
        assert_eq!(m.entry, "builtin_tools");
    }

    #[test]
    fn missing_id_is_error() {
        let text = r#"
name = "x"
kind = "skill"
"#;
        // Missing id either fails deserialization (kind present but id absent
        // → missing required field) or validation; both are errors.
        assert!(parse_manifest(text).is_err());
    }

    #[test]
    fn interaction_kind_wire_name_is_stable() {
        // 前端按字符串分派显示分组：wire 名必须稳定。
        assert_eq!(PluginKind::Interaction.as_str(), "interaction");
        let m = PluginManifest::builtin(
            "echo-agent.example.interaction",
            "交互示例",
            "0.1.0",
            PluginKind::Interaction,
            "example",
            "desc",
        );
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"kind\":\"interaction\""), "{json}");
        let back: PluginManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, PluginKind::Interaction);
    }

    #[test]
    fn roundtrip_serialization() {
        let m = PluginManifest::builtin("a.b.c", "x", "1.0.0", PluginKind::Skill, "entry", "desc");
        let json = serde_json::to_string(&m).unwrap();
        let back: PluginManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
