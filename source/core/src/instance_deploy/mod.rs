//! 适配器实例编排（**通用机制**）+ 平台部署知识（隔离在子模块）。
//!
//! 分两层：
//!
//! | 层 | 内容 | 平台相关性 |
//! |---|---|---|
//! | 本模块 | 实例解析（persona 归属 / 自动建档）、端口分配与探测、共享层⊕实例覆盖合并、TOML 深合并 | **平台中立** |
//! | [`napcat`] | compose 渲染、容器名 / 卷 / 镜像、QQ 三端口语义 | QQ / NapCat 专属 |
//!
//! 这样「加一个平台」只需要读本模块（了解实例与端口模型），不必读懂 NapCat 细节；
//! 迁移平台部署知识时也只搬 `napcat.rs`。
//!
//! 设计要点：
//! - **实例是一等概念**：`instance_id` 全局唯一，`persona` 是归属字段（一个
//!   persona 可挂多个实例）；实例 id 即适配器名与会话 `@account` 维度。
//! - **端口自动分配并持久化**：首次分配后写回
//!   `[instances.<platform>.instances.<id>.ports]`，跨重启稳定（容器端口映射
//!   必须稳定才能让适配器容器重连到同一处）。
//! - **自动创建**：persona 启用了该平台插件且没有归属实例时自动建档
//!   （id = persona，重复则 `<persona>-2`…）。

pub mod napcat;

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::Path;

pub use crate::config::QqPorts;
use crate::config::{InstanceHostConfig, QqInstanceSection};

/// legacy 默认实例名（会话 id 不产生 `@` 后缀）。
///
/// 语义见 `SessionKey::account`：实例名 == 平台名时不写 `@account`。
pub const DEFAULT_INSTANCE: &str = "qq";

/// 一个解析完成的插件实例（配置已合并、端口已确定）。
#[derive(Debug, Clone)]
pub struct PluginInstance {
    pub id: String,
    pub persona: String,
    /// 合并后的完整插件配置（共享层 ⊕ 实例覆盖；TOML 表透传）。
    pub config: toml::Table,
    /// 宿主端口三元组。
    pub ports: QqPorts,
}

/// 端口分配：优先沿用已配置值；缺省时探测空闲端口，并避开本轮已分配给
/// 其他实例的端口（同一次解析里不重复分配）。
///
/// **默认实例（id == 平台名，如 `qq`）走 legacy 语义**：缺省沿用
/// 3131 / 3000 / 6099，且 OneBot HTTP 与 WebUI **不探测**——它们是既有
/// 容器在宿主的端口映射，端口被容器持有，宿主侧 bind 探测必然失败。
pub fn allocate_ports_avoiding(
    existing: &QqPorts,
    id: &str,
    taken: &mut std::collections::HashSet<u16>,
) -> QqPorts {
    let mut ports = existing.clone();
    if id == DEFAULT_INSTANCE {
        if ports.reverse_ws == 0 {
            ports.reverse_ws = if app_port_ok(3131) {
                3131
            } else {
                find_free_port(3140, 3400, taken).unwrap_or(3131)
            };
        }
        if ports.onebot_http == 0 {
            ports.onebot_http = 3000;
        }
        if ports.webui == 0 {
            ports.webui = 6099;
        }
        reserve(&ports, taken);
        return ports;
    }
    // 多实例：全部端口由 Core 分配并持久化（空闲探测，避开本轮已分配）。
    if ports.reverse_ws == 0 || taken.contains(&ports.reverse_ws) {
        ports.reverse_ws = find_free_port(3140, 3400, taken).unwrap_or(0);
    }
    if ports.onebot_http == 0 || taken.contains(&ports.onebot_http) {
        ports.onebot_http = find_free_port(3010, 3090, taken).unwrap_or(0);
    }
    if ports.webui == 0 || taken.contains(&ports.webui) {
        ports.webui = find_free_port(6100, 6190, taken).unwrap_or(0);
    }
    reserve(&ports, taken);
    ports
}

/// 记录实例端口，避免后续实例重复分配。
fn reserve(ports: &QqPorts, taken: &mut std::collections::HashSet<u16>) {
    for port in [ports.reverse_ws, ports.onebot_http, ports.webui] {
        if port > 0 {
            taken.insert(port);
        }
    }
}

fn app_port_ok(port: u16) -> bool {
    if port == 0 {
        return false;
    }
    TcpListener::bind(("0.0.0.0", port)).is_ok()
}

fn find_free_port(start: u16, end: u16, taken: &std::collections::HashSet<u16>) -> Option<u16> {
    (start..end).find(|port| !taken.contains(port) && app_port_ok(*port))
}

/// 合并单实例配置：共享层 ⊕ 实例覆盖（深合并），并派生宿主端口相关键。
///
/// 派生键（**NapCat 部署形状**，键名由插件消费）：
/// - 实例段显式覆盖 → 保留；
/// - 多实例（id ≠ 平台名）未覆盖 → 由 id / 端口派生（多容器互异）；
/// - 默认实例（id == 平台名）未覆盖 → 沿用共享层显式值；共享层缺省时不写
///   （插件默认与迁移前的内置默认一致：容器 `napcat`、localhost:3000/6099）；
/// - 共享层显式置空串 → 派生值兜底。
pub fn merge_instance(
    shared: &toml::Table,
    section: &QqInstanceSection,
    id: &str,
    ports: &QqPorts,
    compose_file: Option<String>,
) -> toml::Table {
    let mut cfg = merge_tables(shared, &section.overrides);
    let auto = id != DEFAULT_INSTANCE;

    // server.bind_address：端口分配结果强制（其余 server 键允许实例覆盖）。
    {
        let server = cfg
            .entry("server")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(table) = server.as_table_mut() {
            table.insert(
                "bind_address".into(),
                toml::Value::String(format!("0.0.0.0:{}", ports.reverse_ws)),
            );
        }
    }

    for (key, derived) in [
        ("napcat_container", format!("echo-napcat-{id}")),
        (
            "napcat_webui_url",
            format!("http://localhost:{}", ports.webui),
        ),
        (
            "napcat_onebot_url",
            format!("http://localhost:{}", ports.onebot_http),
        ),
    ] {
        let in_section = section.overrides.contains_key(key);
        let action: Option<String> = if in_section {
            None
        } else if auto {
            Some(derived)
        } else {
            match cfg.get(key).and_then(toml::Value::as_str) {
                Some(value) if value.trim().is_empty() => Some(derived),
                _ => None,
            }
        };
        if let Some(value) = action {
            cfg.insert(key.into(), toml::Value::String(value));
        }
    }
    if auto {
        cfg.insert(
            "napcat_compose_file".into(),
            toml::Value::String(compose_file.unwrap_or_default()),
        );
    }
    cfg
}

fn merge_tables(base: &toml::Table, overlay: &toml::Table) -> toml::Table {
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

/// 解析实例列表：显式配置的实例 + persona 门控自动创建的实例。
///
/// `enabled_personas` 是启用了该平台适配器插件的人格 id 列表；`data_dir`
/// 已知时把自动生成 compose 的路径写进实例配置（容器编排据此启停容器），
/// `None` 表示不生成 compose 路径。
pub fn resolve_instances_in(
    host: &InstanceHostConfig,
    sections: &BTreeMap<String, QqInstanceSection>,
    enabled_personas: &[String],
    default_persona: &str,
    data_dir: Option<&Path>,
) -> Vec<PluginInstance> {
    let mut out: Vec<PluginInstance> = Vec::new();
    let mut used_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut taken_ports: std::collections::HashSet<u16> = std::collections::HashSet::new();

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
        let ports = allocate_ports_avoiding(&section.ports, id, &mut taken_ports);
        let compose = napcat::auto_compose_path(data_dir, id);
        let config = merge_instance(&host.shared, section, id, &ports, compose);
        used_ids.insert(id.clone());
        out.push(PluginInstance {
            id: id.clone(),
            persona,
            config,
            ports,
        });
    }

    // 2) legacy 单实例快路径：没有实例表但平台启用 → 造一个默认实例。
    if sections.is_empty() && host.enabled {
        let id = DEFAULT_INSTANCE.to_string();
        let section = QqInstanceSection::default();
        let persona = default_persona.to_string();
        let ports = allocate_ports_avoiding(&QqPorts::default(), &id, &mut taken_ports);
        let compose = napcat::auto_compose_path(data_dir, &id);
        let config = merge_instance(&host.shared, &section, &id, &ports, compose);
        used_ids.insert(id.clone());
        out.push(PluginInstance {
            id,
            persona,
            config,
            ports,
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
        // 默认人格的自动实例沿用 legacy id（容器/端口与「没有实例表」时
        // 完全一致）：否则一旦其他人格的实例被写回配置表，legacy 快速路径
        // 不再命中，默认人格的实例会从默认 id 漂移（容器/端口全变）。
        let base = if persona == default_persona {
            DEFAULT_INSTANCE
        } else {
            persona.as_str()
        };
        let id = unique_id(base, &used_ids);
        let section = QqInstanceSection::default();
        let ports = allocate_ports_avoiding(&QqPorts::default(), &id, &mut taken_ports);
        let compose = napcat::auto_compose_path(data_dir, &id);
        let config = merge_instance(&host.shared, &section, &id, &ports, compose);
        used_ids.insert(id.clone());
        out.push(PluginInstance {
            id,
            persona: persona.clone(),
            config,
            ports,
        });
    }

    out
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
    use crate::config::QqSection;

    /// 共享层测试表（等价旧 typed 配置的最小集合）。
    fn shared(enabled: bool) -> InstanceHostConfig {
        shared_with(enabled, &[])
    }

    /// 共享层 + 追加键。
    fn shared_with(enabled: bool, entries: &[(&str, toml::Value)]) -> InstanceHostConfig {
        let mut table = toml::Table::new();
        table.insert("enabled".into(), toml::Value::Boolean(enabled));
        for (key, value) in entries {
            table.insert((*key).into(), value.clone());
        }
        InstanceHostConfig::from_section("qq", &QqSection::default(), table)
    }

    fn config_str<'a>(config: &'a toml::Table, key: &str) -> Option<&'a str> {
        config.get(key).and_then(toml::Value::as_str)
    }

    #[test]
    fn legacy_single_instance_keeps_ports_and_id() {
        let instances = resolve_instances_in(&shared(true), &BTreeMap::new(), &[], "alix", None);
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].id, DEFAULT_INSTANCE);
        assert_eq!(instances[0].persona, "alix");
        // legacy 语义与旧版一致：缺省沿用 3131/3000/6099（OneBot/WebUI 是
        // 既有容器的宿主映射，不探测；反向 WS 若被占用则回退探测其他端口）。
        assert!(instances[0].ports.reverse_ws > 0);
        assert_eq!(instances[0].ports.onebot_http, 3000);
        assert_eq!(instances[0].ports.webui, 6099);
        // bind_address 由端口分配结果强制。
        assert_eq!(
            instances[0].config["server"]["bind_address"]
                .as_str()
                .unwrap(),
            format!("0.0.0.0:{}", instances[0].ports.reverse_ws)
        );
        // 容器 / 地址：共享层缺省时不写覆盖——插件默认（`napcat` /
        // localhost:3000/6099）与迁移前的内置默认一致（P1.9 透传语义）。
        assert!(!instances[0].config.contains_key("napcat_container"));
        assert!(!instances[0].config.contains_key("napcat_onebot_url"));
        assert!(!instances[0].config.contains_key("napcat_webui_url"));
    }

    #[test]
    fn legacy_instance_keeps_shared_container_and_urls() {
        let shared = shared_with(
            true,
            &[
                ("napcat_container", toml::Value::String("my-napcat".into())),
                (
                    "napcat_onebot_url",
                    toml::Value::String("http://10.0.0.5:3000".into()),
                ),
            ],
        );
        let instances = resolve_instances_in(&shared, &BTreeMap::new(), &[], "alix", None);
        assert_eq!(instances.len(), 1);
        assert_eq!(
            config_str(&instances[0].config, "napcat_container"),
            Some("my-napcat")
        );
        assert_eq!(
            config_str(&instances[0].config, "napcat_onebot_url"),
            Some("http://10.0.0.5:3000")
        );
    }

    #[test]
    fn auto_instance_derives_container_and_urls_from_ports() {
        // legacy 归属默认人格 a；b 由自动建档覆盖 → 取 b 断言派生值。
        let personas = vec!["a".to_string(), "b".to_string()];
        let instances = resolve_instances_in(&shared(true), &BTreeMap::new(), &personas, "a", None);
        let auto = instances
            .iter()
            .find(|i| i.id != DEFAULT_INSTANCE)
            .expect("auto instance exists");
        assert_eq!(
            config_str(&auto.config, "napcat_container"),
            Some(format!("echo-napcat-{}", auto.id).as_str())
        );
        assert!(config_str(&auto.config, "napcat_onebot_url")
            .unwrap()
            .contains(&auto.ports.onebot_http.to_string()));
        assert!((3010..3090).contains(&auto.ports.onebot_http));
        assert!((6100..6190).contains(&auto.ports.webui));
    }

    #[test]
    fn auto_creates_instance_per_enabled_persona() {
        let personas = vec!["alix".to_string(), "self-coding".to_string()];
        let instances =
            resolve_instances_in(&shared(true), &BTreeMap::new(), &personas, "alix", None);
        // legacy 实例 + self-coding 自动实例（alix 已被 legacy 覆盖）
        let ids: Vec<&str> = instances.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&DEFAULT_INSTANCE), "ids: {ids:?}");
        assert!(ids.contains(&"self-coding"), "ids: {ids:?}");
    }

    #[test]
    fn two_personas_get_distinct_ports_and_containers() {
        // legacy 实例归属 a（默认人格），b 自动建档 → 恰好 2 个实例
        let personas = vec!["a".to_string(), "b".to_string()];
        let instances = resolve_instances_in(&shared(true), &BTreeMap::new(), &personas, "a", None);
        assert_eq!(instances.len(), 2, "legacy(a) + auto(b)");
        let (x, y) = (&instances[0], &instances[1]);
        assert_ne!(x.ports.reverse_ws, y.ports.reverse_ws);
        assert_ne!(x.ports.onebot_http, y.ports.onebot_http);
        // legacy 不写容器覆盖（插件默认 `napcat`）；auto 派生独立容器名。
        assert_ne!(
            x.config.get("napcat_container"),
            y.config.get("napcat_container")
        );
    }

    #[test]
    fn compose_has_per_instance_container_and_volumes() {
        let ports = QqPorts {
            reverse_ws: 3141,
            onebot_http: 3011,
            webui: 6111,
        };
        let yaml = napcat::render_compose("alix-2", &ports);
        assert!(yaml.contains("container_name: echo-napcat-alix-2"));
        assert!(yaml.contains("\"6111:6099\""));
        assert!(yaml.contains("\"3011:3000\""));
        assert!(yaml.contains("echo-napcat-alix-2-data:"));
    }

    #[test]
    fn default_persona_keeps_legacy_id_when_other_instances_persisted() {
        let shared = shared_with(
            true,
            &[("napcat_container", toml::Value::String("napcat".into()))],
        );
        let mut sections = BTreeMap::new();
        sections.insert(
            "alix".to_string(),
            QqInstanceSection {
                persona: Some("alix".to_string()),
                ports: QqPorts {
                    reverse_ws: 3142,
                    onebot_http: 3011,
                    webui: 6101,
                },
                ..Default::default()
            },
        );
        let instances = resolve_instances_in(
            &shared,
            &sections,
            &["Alice".into(), "alix".into()],
            "Alice",
            None,
        );
        let ids: Vec<&str> = instances.iter().map(|i| i.id.as_str()).collect();
        assert!(
            ids.contains(&DEFAULT_INSTANCE),
            "default persona keeps `qq`: {ids:?}"
        );
        let default = instances
            .iter()
            .find(|i| i.persona == "Alice")
            .expect("Alice instance exists");
        assert_eq!(default.id, DEFAULT_INSTANCE);
        assert_eq!(
            config_str(&default.config, "napcat_container"),
            Some("napcat"),
            "shared container kept"
        );
    }

    #[test]
    fn explicit_instance_wins_over_auto_creation() {
        let mut sections = BTreeMap::new();
        sections.insert(
            "qq2".to_string(),
            QqInstanceSection {
                persona: Some("alix".to_string()),
                ..Default::default()
            },
        );
        let instances =
            resolve_instances_in(&shared(true), &sections, &["alix".to_string()], "x", None);
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].id, "qq2");
        assert_eq!(instances[0].persona, "alix");
    }

    /// P1.9 透传契约：共享层与实例覆盖里的未知 / 未来字段原样存活
    /// （宿主不解析插件 schema）；嵌套表深合并（实例覆盖 server 子键）。
    #[test]
    fn unknown_fields_pass_through_and_deep_merge() {
        let shared = shared_with(
            true,
            &[
                ("fancy_future_option", toml::Value::String("keep-me".into())),
                (
                    "server",
                    toml::Value::Table(toml::Table::from_iter([
                        (
                            "access_token".to_string(),
                            toml::Value::String("shared-token".into()),
                        ),
                        ("heartbeat_interval".to_string(), toml::Value::Integer(30)),
                    ])),
                ),
                ("owner_qq", toml::Value::Integer(10001)),
            ],
        );
        let mut sections = BTreeMap::new();
        sections.insert(
            "qq".to_string(),
            QqInstanceSection {
                persona: Some("alix".to_string()),
                ports: QqPorts {
                    reverse_ws: 3143,
                    onebot_http: 3012,
                    webui: 6102,
                },
                enabled: None,
                overrides: toml::Table::from_iter([
                    ("owner_qq".to_string(), toml::Value::Integer(20002)),
                    (
                        "server".to_string(),
                        toml::Value::Table(toml::Table::from_iter([(
                            "access_token".to_string(),
                            toml::Value::String("instance-token".into()),
                        )])),
                    ),
                ]),
            },
        );
        let instances = resolve_instances_in(&shared, &sections, &[], "alix", None);
        assert_eq!(instances.len(), 1);
        let config = &instances[0].config;
        // 未知字段透传。
        assert_eq!(config_str(config, "fancy_future_option"), Some("keep-me"));
        // 实例覆盖标量。
        assert_eq!(config["owner_qq"].as_integer(), Some(20002));
        // 嵌套深合并：实例的 access_token 覆盖、共享的 heartbeat_interval 保留、
        // bind_address 由端口分配强制。
        assert_eq!(
            config["server"]["access_token"].as_str(),
            Some("instance-token")
        );
        assert_eq!(
            config["server"]["heartbeat_interval"].as_integer(),
            Some(30)
        );
        assert_eq!(
            config["server"]["bind_address"].as_str(),
            Some("0.0.0.0:3143")
        );
    }
}
