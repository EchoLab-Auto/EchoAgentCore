//! 实例解析（组合根侧）：`[instances.<platform>]` → 逐个实例的 id / persona / 覆盖项。
//!
//! **本模块只做与平台无关的三件事**：读实例表、按 persona 门控自动建档、
//! 给出默认人格。**端口分配、容器名、compose 文件全部归插件**——那些是
//! NapCat 的部署形状，Core 不认识（2026-10 去适配器化收尾）。

use std::collections::BTreeMap;

use crate::config::{InstanceHostConfig, PlatformInstanceSection};

/// 默认实例名 = **平台名**（会话 id 不产生 `@` 后缀）。
///
/// 语义见 `SessionKey::account`：实例名 == 平台名时不写 `@account`。
/// 因此这里不需要知道任何具体平台——传 `platform` 进来即可；本模块只是提供
/// 一个语义名，避免调用点各写一遍 `id == platform`。
pub fn default_instance_id(platform: &str) -> &str {
    platform
}

/// 一个解析完成的实例（配置已合并、端口/容器由插件自行决定）。
#[derive(Debug, Clone)]
pub struct ResolvedInstance {
    pub id: String,
    pub persona: String,
    /// 实例级覆盖片段（原样交给插件；宿主不解释）。
    pub overrides: toml::Table,
}

/// 解析实例列表：显式配置的实例 + persona 门控自动创建的实例。
///
/// `enabled_personas` 是启用了该平台插件的人格 id 列表。
pub fn resolve_instances(
    platform: &str,
    host: &InstanceHostConfig,
    sections: &BTreeMap<String, PlatformInstanceSection>,
    enabled_personas: &[String],
    default_persona: &str,
) -> Vec<ResolvedInstance> {
    let mut out: Vec<ResolvedInstance> = Vec::new();
    let mut used_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 1) 显式实例（按配置顺序）。
    for (id, section) in sections {
        let persona = section
            .persona
            .clone()
            .unwrap_or_else(|| default_persona.to_string());
        let enabled = section.enabled.unwrap_or(host.enabled);
        if !enabled {
            continue;
        }
        used_ids.insert(id.clone());
        out.push(ResolvedInstance {
            id: id.clone(),
            persona,
            overrides: section.overrides.clone(),
        });
    }

    // 2) legacy 单实例快路径：没有实例表但平台启用 → 造一个默认实例。
    if sections.is_empty() && host.enabled {
        let id = default_instance_id(platform);
        used_ids.insert(id.to_string());
        out.push(ResolvedInstance {
            id: id.to_string(),
            persona: default_persona.to_string(),
            overrides: toml::Table::new(),
        });
    }

    // 3) persona 门控自动建档：启用了插件但没有任何归属实例 → 按 persona 建。
    if !host.enabled {
        return out;
    }
    for persona in enabled_personas {
        if out.iter().any(|instance| &instance.persona == persona) {
            continue;
        }
        // 默认人格的自动实例沿用 legacy id：否则一旦其他人格的实例被写回
        // 配置表，legacy 快速路径不再命中，默认人格的实例会从默认 id 漂移。
        let base = if persona == default_persona {
            default_instance_id(platform)
        } else {
            persona.as_str()
        };
        let id = unique_id(base, &used_ids);
        used_ids.insert(id.clone());
        out.push(ResolvedInstance {
            id,
            persona: persona.clone(),
            overrides: toml::Table::new(),
        });
    }

    out
}

/// 深合并两张 TOML 表（实例覆盖 ⊕ 共享层）——留给需要预览合并结果的调用方
/// （宿主只做透传，实际合并与派生由插件完成）。
#[cfg_attr(not(test), allow(dead_code))]
/// 写回实例配置时该用哪个段名。
///
/// Core 读取时 `[instances.<platform>]` 与 `[adapters.<platform>]` 是同一个字段的
/// 两个 `serde(alias)`（`config::CoreConfig::instances_section`），但**写回必须沿用
/// 用户已经在用的那个名字**：若用户按去适配器化后的 `[instances.qq]` 配置，写回时
/// 硬造一个 `adapters` 段会让配置文件里同时出现两个段——下次启动 serde 直接报
/// `duplicate field`，Core 起不来（自动建档触发写回 → 重启即崩）。
///
/// 两段都不存在时用 `adapters`（与出厂模板一致）。
pub fn instance_section_key(root: &toml::Table, platform: &str) -> &'static str {
    let has_platform = |key: &str| root.get(key).and_then(|v| v.get(platform)).is_some();
    if has_platform("adapters") {
        "adapters"
    } else if has_platform("instances") {
        "instances"
    } else {
        "adapters"
    }
}

pub fn merge_tables(base: &toml::Table, overlay: &toml::Table) -> toml::Table {
    let mut out = base.clone();
    for (key, value) in overlay {
        match (out.get_mut(key), value) {
            (Some(toml::Value::Table(inner)), toml::Value::Table(over)) => {
                let merged = merge_tables(inner, over);
                *inner = merged;
            }
            _ => {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    out
}

/// 兼容旧插件的 `config` 字段：共享层 ⊕ 实例覆盖（**不派生**端口/容器名）。
///
/// 新插件改用 `shared` + `overrides` 自己合并派生；这个合并结果只为
/// 「新 Core + 已部署的旧插件」过渡窗口存在（旧插件读 `config`）。
pub fn legacy_merged_config(host: &InstanceHostConfig, overrides: &toml::Table) -> toml::Table {
    merge_tables(&host.shared, overrides)
}

fn unique_id(base: &str, used: &std::collections::HashSet<String>) -> String {
    if !used.contains(base) {
        return base.to_string();
    }
    for suffix in 2..100 {
        let candidate = format!("{base}-{suffix}");
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    format!("{base}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PlatformSection;

    fn host(enabled: bool, instances: &[(&str, Option<&str>)]) -> InstanceHostConfig {
        let mut sections = BTreeMap::new();
        for (id, persona) in instances {
            sections.insert(
                id.to_string(),
                PlatformInstanceSection {
                    persona: persona.map(str::to_string),
                    enabled: None,
                    overrides: toml::Table::new(),
                },
            );
        }
        let section = PlatformSection {
            shared: {
                let mut t = toml::Table::new();
                t.insert("enabled".into(), toml::Value::Boolean(enabled));
                t
            },
            instances: sections,
            ..PlatformSection::default()
        };
        InstanceHostConfig::from_section("qq", &section, section.shared.clone())
    }

    #[test]
    fn explicit_instances_keep_configured_persona_and_order() {
        let h = host(true, &[("a", Some("alix")), ("b", Some("bob"))]);
        let resolved = resolve_instances("qq", &h, &h.instances, &[], "alix");
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].id, "a");
        assert_eq!(resolved[0].persona, "alix");
        assert_eq!(resolved[1].persona, "bob");
    }

    #[test]
    fn legacy_single_instance_when_no_table() {
        let h = host(true, &[]);
        let resolved = resolve_instances("qq", &h, &h.instances, &[], "alix");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, "qq");
        assert_eq!(resolved[0].persona, "alix");
    }

    #[test]
    fn auto_creates_one_instance_per_enabled_persona() {
        let h = host(true, &[]);
        let resolved = resolve_instances(
            "qq",
            &h,
            &h.instances,
            &["alix".into(), "bob".into()],
            "alix",
        );
        assert_eq!(resolved.len(), 2);
        // 默认人格沿用 legacy id；其余人格用自己的 id。
        assert_eq!(resolved[0].id, "qq");
        assert_eq!(resolved[1].id, "bob");
    }

    #[test]
    fn disabled_platform_yields_nothing() {
        let h = host(false, &[]);
        let resolved = resolve_instances("qq", &h, &h.instances, &[], "alix");
        assert!(resolved.is_empty());
    }

    #[test]
    fn overrides_pass_through_untouched() {
        let mut overrides = toml::Table::new();
        overrides.insert(
            "napcat_container".into(),
            toml::Value::String("mine".into()),
        );
        let mut sections = BTreeMap::new();
        sections.insert(
            "alix".to_string(),
            PlatformInstanceSection {
                persona: Some("alix".into()),
                enabled: None,
                overrides: overrides.clone(),
            },
        );
        let mut h = host(true, &[]);
        h.instances = sections.clone();
        let resolved = resolve_instances("qq", &h, &sections, &[], "alix");
        assert_eq!(
            resolved[0].overrides["napcat_container"].as_str(),
            Some("mine")
        );
    }

    #[test]
    fn write_back_keeps_the_section_name_the_user_already_uses() {
        // 用户配 [instances.qq] → 写回必须还在 instances 段（否则重启时 serde
        // 同时看到两段会报 duplicate field，Core 起不来）
        let root: toml::Table = "[instances.qq]\nenabled = true\n".parse().unwrap();
        assert_eq!(instance_section_key(&root, "qq"), "instances");

        // 用户配 [adapters.qq]（出厂模板）→ 保持 adapters
        let root: toml::Table = "[adapters.qq]\nenabled = true\n".parse().unwrap();
        assert_eq!(instance_section_key(&root, "qq"), "adapters");

        // 段在但平台不在（如只有 [instances.telegram]）→ 回落模板默认
        let root: toml::Table = "[instances.telegram]\nenabled = true\n".parse().unwrap();
        assert_eq!(instance_section_key(&root, "qq"), "adapters");

        // 空配置 → 模板默认
        let root: toml::Table = "".parse().unwrap();
        assert_eq!(instance_section_key(&root, "qq"), "adapters");
    }
}
