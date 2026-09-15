//! QQ 多实例：配置解析、端口分配、容器编排。
//!
//! 设计（2026-09）：
//! - **实例是一等概念**：`instance_id` 全局唯一，`persona` 是归属字段
//!   （一个 persona 可挂多个实例）；实例 id 即适配器名与会话 `@account` 维度。
//! - **端口自动分配并持久化**：每实例 3 个宿主端口（反向 WS / OneBot HTTP /
//!   WebUI）。首次分配后写入 `[adapters.qq.instances.<id>.ports]`，跨重启稳定
//!   （容器端口映射必须稳定才能让 NapCat 重连到同一处）。
//! - **容器编排**：每实例一个 compose 文件（独立容器名与数据卷、独立端口映射），
//!   落在 `~/.local/share/echo-agent-core/napcat/<id>/docker-compose.yml`。
//! - **自动创建**：persona 启用 `echo-agent.adapter.qq` 且没有归属实例时自动建
//!   档（id = persona，重复则 `<persona>-2`…）。
//! - 单实例（id = `qq`）时端口沿用 legacy 3131/3000/6099，行为与旧版一致。

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use crate::config::QqInstanceSection;
pub use crate::config::QqPorts;

/// legacy 默认实例名（会话 id 不产生 `@` 后缀）。
pub const DEFAULT_INSTANCE: &str = "qq";

/// 一个解析完成的 QQ 实例（配置已合并、端口已确定）。
#[derive(Debug, Clone)]
pub struct QqInstance {
    /// 实例 id（= 适配器名 = 会话 account 维度）。
    pub id: String,
    /// 归属人格 id。
    pub persona: String,
    /// 合并后的适配器配置（含该实例的端口与容器名）。
    pub config: echo_adapter_qq::QqAdapterConfig,
    /// 宿主端口三元组。
    pub ports: QqPorts,
}

/// 端口分配：优先沿用已配置值；缺省时探测空闲端口，
/// 并避开本轮已分配给其他实例的端口（同一次解析里不重复分配）。
pub fn allocate_ports_avoiding(
    existing: &QqPorts,
    id: &str,
    taken: &std::collections::HashSet<u16>,
) -> QqPorts {
    let mut ports = existing.clone();
    if id == DEFAULT_INSTANCE {
        // legacy 实例（`qq`）：语义与旧版一致，缺省沿用 3131/3000/6099。
        // - 反向 WS 由 Core 自身 bind：3131 可绑定则保留（否则回退探测）；
        // - OneBot HTTP / WebUI 是既有 NapCat 容器的宿主映射（端口被容器
        //   持有，宿主侧 bind 探测必然失败），缺省**直接沿用、不探测**。
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
    ports
}

fn app_port_ok(port: u16) -> bool {
    if port == 0 {
        return false;
    }
    TcpListener::bind(("0.0.0.0", port)).is_ok()
}

/// 在 [start, end) 里找第一个可绑定且未被本轮占用的端口。
fn find_free_port(start: u16, end: u16, taken: &std::collections::HashSet<u16>) -> Option<u16> {
    (start..end).find(|p| !taken.contains(p) && app_port_ok(*p))
}

/// 把实例配置合并到共享默认之上（persona 相关字段逐个覆盖）。
pub fn merge_instance(
    shared: &echo_adapter_qq::QqAdapterConfig,
    section: &QqInstanceSection,
    id: &str,
    persona: &str,
    ports: &QqPorts,
    compose_file: Option<String>,
) -> echo_adapter_qq::QqAdapterConfig {
    let mut cfg = shared.clone();
    cfg.server.bind_address = format!("0.0.0.0:{}", ports.reverse_ws);
    if let Some(server) = &section.server {
        // 端口以外允许覆盖（token/心跳），但 bind_address 以端口分配为准。
        cfg.server.access_token = server.access_token.clone();
        cfg.server.heartbeat_interval = server.heartbeat_interval;
    }
    if let Some(owner) = section.owner_qq {
        cfg.owner_qq = owner;
    }
    let auto = id != DEFAULT_INSTANCE;
    // legacy 实例的容器名与 NapCat 地址是"既有部署"的一部分：实例段未覆盖时
    // 必须沿用共享默认（container 默认 `napcat`、地址默认 localhost:3000/6099），
    // 否则会把 docker 管理指向不存在的新容器名。
    let shared_or = |chosen: Option<String>, shared_value: &str| -> Option<String> {
        chosen.or_else(|| {
            if !auto && !shared_value.trim().is_empty() {
                Some(shared_value.to_string())
            } else {
                None
            }
        })
    };
    cfg.napcat_container = shared_or(section.napcat_container.clone(), &shared.napcat_container)
        .unwrap_or_else(|| format!("echo-napcat-{id}"));
    cfg.napcat_webui_url = shared_or(section.napcat_webui_url.clone(), &shared.napcat_webui_url)
        .unwrap_or_else(|| format!("http://localhost:{}", ports.webui));
    cfg.napcat_onebot_url = shared_or(section.napcat_onebot_url.clone(), &shared.napcat_onebot_url)
        .unwrap_or_else(|| format!("http://localhost:{}", ports.onebot_http));
    // 多实例容器由 Core 托管：自动启停按共享默认（除非显式覆盖）。
    cfg.napcat_auto_start = section
        .napcat_auto_start
        .unwrap_or(shared.napcat_auto_start);
    cfg.napcat_auto_stop = section.napcat_auto_stop.unwrap_or(shared.napcat_auto_stop);
    if auto {
        // 多实例：compose 由 Core 生成（路径由调用方给出）。
        cfg.napcat_compose_file = compose_file.unwrap_or_default();
    }
    let _ = persona;
    cfg
}

/// 生成实例的 compose 文件内容（独立容器名 / 端口 / 数据卷）。
pub fn render_compose(id: &str, ports: &QqPorts) -> String {
    format!(
        r#"# 由 EchoAgentCore 自动生成（QQ 实例 `{id}`）——请勿手工编辑。
# 端口映射由 [adapters.qq.instances.{id}.ports] 决定。
services:
  napcat:
    image: mlikiowa/napcat-docker:latest
    container_name: echo-napcat-{id}
    restart: unless-stopped
    ports:
      - "{webui}:6099"      # WebUI（扫码登录）
      - "{onebot}:3000"     # HTTP API
    environment:
      - NAPCAT_UID=0
      - NAPCAT_GID=0
    extra_hosts:
      - "host.docker.internal:host-gateway"
    volumes:
      - echo-napcat-{id}-data:/app/napcat/data
      - echo-napcat-{id}-config:/app/napcat/config

volumes:
  echo-napcat-{id}-data:
  echo-napcat-{id}-config:
"#,
        id = id,
        webui = ports.webui,
        onebot = ports.onebot_http,
    )
}

/// 写实例 compose 文件（幂等；内容变化才写）。
pub fn write_compose(data_dir: &Path, id: &str, ports: &QqPorts) -> Result<PathBuf, String> {
    let dir = data_dir.join("napcat").join(id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join("docker-compose.yml");
    let content = render_compose(id, ports);
    let unchanged = std::fs::read_to_string(&path)
        .map(|old| old == content)
        .unwrap_or(false);
    if !unchanged {
        std::fs::write(&path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(path)
}

/// 解析实例列表：显式配置的实例 + persona 门控自动创建的实例。
///
/// `enabled_personas` 是启用了 `echo-agent.adapter.qq` 插件的人格 id 列表；
/// `data_dir` 已知时把自动生成 compose 的路径写进实例配置（容器编排据此
/// 启停 NapCat），None 表示不生成 compose 路径。
pub fn resolve_instances_in(
    shared: &echo_adapter_qq::QqAdapterConfig,
    sections: &BTreeMap<String, QqInstanceSection>,
    enabled_personas: &[String],
    default_persona: &str,
    data_dir: Option<&Path>,
) -> Vec<QqInstance> {
    let mut out: Vec<QqInstance> = Vec::new();
    let mut used_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut taken_ports: std::collections::HashSet<u16> = std::collections::HashSet::new();

    /// 记录实例端口，避免后续实例重复分配。
    fn reserve(ports: &QqPorts, taken: &mut std::collections::HashSet<u16>) {
        for p in [ports.reverse_ws, ports.onebot_http, ports.webui] {
            if p > 0 {
                taken.insert(p);
            }
        }
    }

    // 1) 显式实例
    for (id, section) in sections {
        let enabled = section.enabled.unwrap_or(shared.enabled);
        if !enabled {
            continue;
        }
        let persona = section
            .persona
            .clone()
            .unwrap_or_else(|| default_persona.to_string());
        let ports = allocate_ports_avoiding(&section.ports, id, &taken_ports);
        reserve(&ports, &mut taken_ports);
        let compose = auto_compose_path(data_dir, id);
        let config = merge_instance(shared, section, id, &persona, &ports, compose);
        used_ids.insert(id.clone());
        out.push(QqInstance {
            id: id.clone(),
            persona,
            config,
            ports,
        });
    }

    // 2) legacy 快速路径：没有实例表但共享默认 enabled → 单实例 `qq`
    //（有任何显式实例时跳过；默认人格的 `qq` 由下方自动建档兜底创建）
    if sections.is_empty() && shared.enabled {
        let ports = allocate_ports_avoiding(&QqPorts::default(), DEFAULT_INSTANCE, &taken_ports);
        reserve(&ports, &mut taken_ports);
        let config = merge_instance(
            shared,
            &QqInstanceSection::default(),
            DEFAULT_INSTANCE,
            default_persona,
            &ports,
            None,
        );
        out.push(QqInstance {
            id: DEFAULT_INSTANCE.into(),
            persona: default_persona.to_string(),
            config,
            ports,
        });
    }

    // 3) persona 门控自动创建（启用插件但无归属实例）。
    // 前提：QQ 适配器全局启用（`[adapters.qq].enabled`）——否则不该有实例。
    if !shared.enabled {
        return out;
    }
    for persona in enabled_personas {
        if out.iter().any(|i| &i.persona == persona) {
            continue;
        }
        // 默认人格的自动实例沿用 legacy id `qq`：容器沿用共享默认（`napcat`）、
        // 端口沿用 3131/3000/6099，且**不写回配置**——与"没有实例表"时的
        // legacy 语义完全一致。否则一旦其他人格的实例被写回配置表，
        // legacy 快速路径（sections 为空）不再命中，默认人格的实例会从
        // `qq` 漂移为新实例（容器/端口全变）。
        let base = if persona == default_persona {
            DEFAULT_INSTANCE
        } else {
            persona.as_str()
        };
        let id = unique_id(base, &used_ids);
        used_ids.insert(id.clone());
        let ports = allocate_ports_avoiding(&QqPorts::default(), &id, &taken_ports);
        reserve(&ports, &mut taken_ports);
        let compose = auto_compose_path(data_dir, &id);
        let config = merge_instance(
            shared,
            &QqInstanceSection::default(),
            &id,
            persona,
            &ports,
            compose,
        );
        out.push(QqInstance {
            id,
            persona: persona.clone(),
            config,
            ports,
        });
    }

    out
}

/// 自动生成的 compose 路径（data_dir 已知时）。
fn auto_compose_path(data_dir: Option<&Path>, id: &str) -> Option<String> {
    data_dir.map(|dir| {
        dir.join("napcat")
            .join(id)
            .join("docker-compose.yml")
            .to_string_lossy()
            .to_string()
    })
}

fn unique_id(base: &str, used: &std::collections::HashSet<String>) -> String {
    if !used.contains(base) {
        return base.to_string();
    }
    (2..100)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !used.contains(candidate))
        .unwrap_or_else(|| format!("{base}-x"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_single_instance_keeps_ports_and_id() {
        let mut shared = echo_adapter_qq::QqAdapterConfig::default();
        shared.enabled = true;
        shared.napcat_auto_start = false;
        let instances = resolve_instances_in(&shared, &BTreeMap::new(), &[], "alix", None);
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].id, DEFAULT_INSTANCE);
        assert_eq!(instances[0].persona, "alix");
        // legacy 语义与旧版一致：缺省沿用 3131/3000/6099（OneBot/WebUI 是
        // 既有容器的宿主映射，不探测；反向 WS 若被占用则回退探测其他端口）。
        assert!(instances[0].ports.reverse_ws > 0);
        assert_eq!(instances[0].ports.onebot_http, 3000);
        assert_eq!(instances[0].ports.webui, 6099);
        // 容器与地址沿用共享默认（docker 管理必须指向既有容器 `napcat`）。
        assert_eq!(instances[0].config.napcat_container, "napcat");
        assert!(instances[0].config.napcat_onebot_url.contains(":3000"));
        assert!(instances[0].config.napcat_webui_url.contains(":6099"));
    }

    #[test]
    fn legacy_instance_keeps_shared_container_and_urls() {
        let mut shared = echo_adapter_qq::QqAdapterConfig::default();
        shared.enabled = true;
        shared.napcat_auto_start = false;
        shared.napcat_container = "my-napcat".into();
        shared.napcat_onebot_url = "http://10.0.0.5:3000".into();
        let instances = resolve_instances_in(&shared, &BTreeMap::new(), &[], "alix", None);
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].config.napcat_container, "my-napcat");
        assert_eq!(
            instances[0].config.napcat_onebot_url,
            "http://10.0.0.5:3000"
        );
    }

    #[test]
    fn auto_instance_derives_container_and_urls_from_ports() {
        let mut shared = echo_adapter_qq::QqAdapterConfig::default();
        shared.enabled = true;
        shared.napcat_auto_start = false;
        // legacy 归属默认人格 a；b 由自动建档覆盖 → 取 b 断言派生值。
        let personas = vec!["a".to_string(), "b".to_string()];
        let instances = resolve_instances_in(&shared, &BTreeMap::new(), &personas, "a", None);
        let auto = instances
            .iter()
            .find(|i| i.id != DEFAULT_INSTANCE)
            .expect("auto instance exists");
        assert_eq!(
            auto.config.napcat_container,
            format!("echo-napcat-{}", auto.id)
        );
        assert!(auto
            .config
            .napcat_onebot_url
            .contains(&auto.ports.onebot_http.to_string()));
        assert!((3010..3090).contains(&auto.ports.onebot_http));
        assert!((6100..6190).contains(&auto.ports.webui));
    }

    #[test]
    fn auto_creates_instance_per_enabled_persona() {
        let mut shared = echo_adapter_qq::QqAdapterConfig::default();
        shared.enabled = true;
        shared.napcat_auto_start = false;
        let personas = vec!["alix".to_string(), "self-coding".to_string()];
        let instances = resolve_instances_in(&shared, &BTreeMap::new(), &personas, "alix", None);
        // legacy 实例 + self-coding 自动实例（alix 已被 legacy 覆盖）
        let ids: Vec<&str> = instances.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&DEFAULT_INSTANCE), "ids: {ids:?}");
        assert!(ids.contains(&"self-coding"), "ids: {ids:?}");
    }

    #[test]
    fn two_personas_get_distinct_ports_and_containers() {
        let mut shared = echo_adapter_qq::QqAdapterConfig::default();
        shared.enabled = true;
        shared.napcat_auto_start = false;
        // legacy 实例归属 a（默认人格），b 自动建档 → 恰好 2 个实例
        let personas = vec!["a".to_string(), "b".to_string()];
        let instances = resolve_instances_in(&shared, &BTreeMap::new(), &personas, "a", None);
        assert_eq!(instances.len(), 2, "legacy(a) + auto(b)");
        let (x, y) = (&instances[0], &instances[1]);
        assert_ne!(x.ports.reverse_ws, y.ports.reverse_ws);
        assert_ne!(x.ports.onebot_http, y.ports.onebot_http);
        assert_ne!(x.config.napcat_container, y.config.napcat_container);
    }

    #[test]
    fn compose_has_per_instance_container_and_volumes() {
        let ports = QqPorts {
            reverse_ws: 3141,
            onebot_http: 3011,
            webui: 6111,
        };
        let yaml = render_compose("alix-2", &ports);
        assert!(yaml.contains("container_name: echo-napcat-alix-2"));
        assert!(yaml.contains("\"6111:6099\""));
        assert!(yaml.contains("\"3011:3000\""));
        assert!(yaml.contains("echo-napcat-alix-2-data:"));
    }

    /// 防漂移：显式实例（其他人格）存在时，默认人格的自动实例仍沿用 legacy
    /// id `qq` 与共享容器/端口——否则"首个实例被挤出配置表"后归属会漂移。
    #[test]
    fn default_persona_keeps_legacy_id_when_other_instances_persisted() {
        let mut shared = echo_adapter_qq::QqAdapterConfig::default();
        shared.enabled = true;
        shared.napcat_auto_start = false;
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
            default.config.napcat_container, "napcat",
            "shared container kept"
        );
    }

    #[test]
    fn explicit_instance_wins_over_auto_creation() {
        let mut shared = echo_adapter_qq::QqAdapterConfig::default();
        shared.enabled = true;
        shared.napcat_auto_start = false;
        let mut sections = BTreeMap::new();
        sections.insert(
            "qq2".to_string(),
            QqInstanceSection {
                persona: Some("alix".to_string()),
                ..Default::default()
            },
        );
        let instances = resolve_instances_in(&shared, &sections, &["alix".to_string()], "x", None);
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].id, "qq2");
        assert_eq!(instances[0].persona, "alix");
    }
}
