//! `echo-agent-core port` 子命令：查看 / 修改各监听端口（2026-10）。
//!
//! 与 `token` 子命令同款策略：**直接操作 TOML 文本**（保留注释与其他段，
//! 不做整树重写）；写回后需重启 Core 生效。
//!
//! 目标键 ↔ 配置键（`port` 列表第一列即目标键，可直接复制给 `port set`）：
//!
//! | target | 配置键 | 说明 |
//! |---|---|---|
//! | `management` | `[core].management_address` | Panel management WS 监听（host:port） |
//! | `federation` | `[federation].listen` | 联邦监听（host:port；**缺省 = 合并至管理端口**；`none` = 仅连出） |
//! | `federation-advertise` | `[federation].advertise` | 邀请串对外宣告地址（host[:port]） |
//! | `qq` | `[instances.qq.instances.qq.ports].reverse_ws` | 默认 QQ 实例反向 WS（Core 自绑定） |
//!
//! 注意：`[instances.qq.server].bind_address` 在运行时会被实例端口分配
//! 覆盖（`qq_instances::merge_instance`），**实际监听端口以实例 `ports`
//! 三元组为准**——本工具因此只写权威键。
//!
//! 值形态：
//! - 地址类（management / federation*）：`<端口>`（沿用现有 host，缺省
//!   host 为 `0.0.0.0`）或 `<host>:<端口>`；federation* 另接受 `none`
//!   （写空串：listen 空 = 仅连出、advertise 空 = 按 listen 推导）。
//! - 实例端口：`<端口>` 或 `auto`（写 0 = 下次启动重新自动分配）。

use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;

/// 各端口的缺省值（与 config.rs / qq_instances.rs 的运行时缺省一致）。
pub const DEFAULT_MANAGEMENT: &str = "0.0.0.0:3132";
pub const DEFAULT_FEDERATION_LISTEN: &str = "0.0.0.0:3133";

/// `echo-agent-core port` 的次级动作。
#[derive(Debug, Subcommand)]
pub enum PortSubCommand {
    /// 列出全部端口（缺省动作）。
    List,
    /// 修改端口 / 监听地址（重启 Core 生效）。
    Set {
        /// 目标键：management | federation | federation-advertise | qq |
        /// （直接复制 `port` 列表第一列）。
        target: String,
        /// 新值：<端口> / <host:端口> / none（仅 federation*）/ auto（仅实例端口）。
        value: String,
    },
}

/// 运行 `port` 子命令。
pub fn run(config_path: &Path, action: Option<PortSubCommand>) -> Result<()> {
    match action.unwrap_or(PortSubCommand::List) {
        PortSubCommand::List => list(config_path),
        PortSubCommand::Set { target, value } => set(config_path, &target, &value),
    }
}

// ── 目标解析 ───────────────────────────────────────────────────────────────

/// 修改目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Management,
    FederationListen,
    FederationAdvertise,
}

impl Target {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "management" => return Ok(Self::Management),
            "federation" => return Ok(Self::FederationListen),
            "federation-advertise" => return Ok(Self::FederationAdvertise),
            _ => {}
        }
        Err(format!(
            "未知目标 `{s}`（可选：management | federation | federation-advertise）"
        ))
    }

    /// 目标对应的 (段头, 键)。
    fn section_key(&self) -> (String, String) {
        match self {
            Self::Management => ("core".to_string(), "management_address".to_string()),
            Self::FederationListen => ("federation".to_string(), "listen".to_string()),
            Self::FederationAdvertise => ("federation".to_string(), "advertise".to_string()),
        }
    }
}

/// 实例 id 卫生：仅 `[A-Za-z0-9_-]`（防经段头注入 TOML；与 persona id 约定一致）。

// ── 值解析 ─────────────────────────────────────────────────────────────────

/// 端口解析：1-65535（0 由 `auto` 关键字单独表达）。
fn parse_port(s: &str) -> Result<u16, String> {
    let value: u32 = s
        .parse()
        .map_err(|_| format!("端口 `{s}` 非法（需为数字）"))?;
    if !(1..=65535).contains(&value) {
        return Err(format!("端口 `{s}` 超出范围（1-65535）"));
    }
    Ok(value as u16)
}

/// 拆分 `host:port`（兼容 IPv6 `[addr]:port`）。
fn split_host_port(s: &str) -> Option<(&str, u16)> {
    let (host, port) = if let Some(stripped) = s.strip_prefix('[') {
        // `[addr]:port`：`]` 在原始串中的下标 = stripped 中 `]` 下标 + 1。
        let idx = stripped.find("]:")?;
        (&s[..idx + 2], &stripped[idx + 2..])
    } else {
        s.rsplit_once(':')?
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port.parse().ok()?))
}

/// host 部分卫生（非空、无空白/引号）。
fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("地址缺少主机部分".to_string());
    }
    if host
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '#')
    {
        return Err(format!("主机部分 `{host}` 含非法字符"));
    }
    Ok(())
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// 归一化地址类值（management / federation listen）。
///
/// - `<端口>`：沿用当前值的 host（缺省 `0.0.0.0`）；
/// - `<host>:<端口>`：整体替换（校验）；
/// - `none`：仅当 `allow_none` 时写空串（federation 语义）。
fn normalize_addr(value: &str, current: &str, allow_none: bool) -> Result<String, String> {
    let value = value.trim();
    if value == "none" {
        if allow_none {
            return Ok(String::new());
        }
        return Err("该端口不支持 none（必须监听）".to_string());
    }
    if is_digits(value) {
        let port = parse_port(value)?;
        let host = split_host_port(current.trim())
            .map(|(h, _)| h.to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "0.0.0.0".to_string());
        return Ok(format!("{host}:{port}"));
    }
    let (host, port) = split_host_port(value)
        .ok_or_else(|| format!("地址 `{value}` 非法（需为 <端口> 或 <host>:<端口>）"))?;
    validate_host(host)?;
    parse_port(&port.to_string())?;
    Ok(format!("{host}:{port}"))
}

/// 归一化 advertise 值：`host` / `host:port` / `none` / `<端口>`（沿用现有 host）。
fn normalize_advertise(value: &str, current: &str) -> Result<String, String> {
    let value = value.trim();
    if value == "none" {
        return Ok(String::new());
    }
    if is_digits(value) {
        let port = parse_port(value)?;
        let host = split_host_port(current.trim())
            .map(|(h, _)| h.to_string())
            .filter(|h| !h.is_empty())
            .ok_or_else(|| {
                "advertise 需要主机部分（当前值为空——请填 <host> 或 <host>:<端口>）".to_string()
            })?;
        return Ok(format!("{host}:{port}"));
    }
    if let Some((host, port)) = split_host_port(value) {
        validate_host(host)?;
        parse_port(&port.to_string())?;
        return Ok(format!("{host}:{port}"));
    }
    // 纯 host（advertise 允许省略端口 → 采用 listen 端口）。
    validate_host(value)?;
    Ok(value.to_string())
}

/// 归一化实例端口值：`<端口>` 或 `auto`（→ 0）。

// ── 文本编辑 ───────────────────────────────────────────────────────────────

/// 解析表头行 → (归一化段名, 是否数组表 `[[...]]`)。
fn parse_header(line: &str) -> Option<(String, bool)> {
    let trimmed = line.trim_start();
    let (inner, is_array) = if let Some(rest) = trimmed.strip_prefix("[[") {
        let end = rest.find("]]")?;
        (&rest[..end], true)
    } else {
        let rest = trimmed.strip_prefix('[')?;
        let end = rest.find(']')?;
        (&rest[..end], false)
    };
    let normalized = inner
        .split('.')
        .map(|part| part.trim().trim_matches('"'))
        .collect::<Vec<_>>()
        .join(".");
    Some((normalized, is_array))
}

/// 在行区间内找 `key = ...` 行（跳过注释；不做前缀误命中）。
fn find_key(lines: &[String], from: usize, to: usize, key: &str) -> Option<usize> {
    for (offset, line) in lines[from..to].iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(key) {
            if rest.trim_start().starts_with('=') {
                return Some(from + offset);
            }
        }
    }
    None
}

/// 定位段的行区间 `[start, end)`（end = 下一个表头处或文件尾）。
fn locate_section(lines: &[String], section: &str) -> Option<(usize, usize)> {
    let mut start = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some((name, _)) = parse_header(line) {
            if let Some(s) = start {
                return Some((s, i));
            }
            if name == section {
                start = Some(i);
            }
        }
    }
    start.map(|s| (s, lines.len()))
}

/// 在 `at` 处插入块（自动补前后空行分隔）。
fn insert_block(lines: &mut Vec<String>, at: usize, mut block: Vec<String>) {
    let needs_leading_blank = at > 0 && !lines[at - 1].trim().is_empty();
    let needs_trailing_blank = at < lines.len() && !lines[at].trim().is_empty();
    if needs_trailing_blank {
        block.push(String::new());
    }
    let mut positioned = if needs_leading_blank {
        let mut v = vec![String::new()];
        v.extend(block);
        v
    } else {
        block
    };
    lines.splice(at..at, positioned.drain(..));
}

/// 设置 `<section>` 内的 `key = "<value>"`（字符串形态）。
fn set_string_key(text: &str, section: &str, key: &str, value: &str) -> Result<String, String> {
    if value.contains('"') || value.contains('\n') || value.contains('\r') {
        return Err("值含非法字符（引号或换行）".to_string());
    }
    set_key_line(text, section, key, &format!("{key} = \"{value}\""))
}

/// 设置 `<section>` 内的 `key = <整数>`（裸值形态；QQ 实例端口用）。

/// 设置 `<section>` 内的 `key = ...`（赋值行完整给出）：
/// - 段内已有该键：原位替换；
/// - 段存在但键缺失：插到段尾（下一个表头之前）；
/// - 段不存在：优先插在其子表之前 → 最近祖先段尾 → 文件尾（TOML 均合法）。
fn set_key_line(text: &str, section: &str, key: &str, new_line: &str) -> Result<String, String> {
    let crlf = text.contains("\r\n");
    let mut lines: Vec<String> = text
        .replace("\r\n", "\n")
        .split('\n')
        .map(str::to_string)
        .collect();

    if let Some((start, end)) = locate_section(&lines, section) {
        match find_key(&lines, start + 1, end, key) {
            Some(idx) => {
                let indent_len = lines[idx].len() - lines[idx].trim_start().len();
                let indent = lines[idx][..indent_len].to_string();
                lines[idx] = format!("{indent}{new_line}");
            }
            None => insert_block(&mut lines, end, vec![new_line.to_string()]),
        }
    } else {
        // 段不存在：按「子表之前 → 祖先段尾 → 文件尾」优先级落位。
        let descendant_prefix = format!("{section}.");
        let descendant = lines.iter().position(|line| {
            parse_header(line)
                .map(|(name, _)| name.starts_with(&descendant_prefix))
                .unwrap_or(false)
        });
        if let Some(idx) = descendant {
            insert_block(
                &mut lines,
                idx,
                vec![format!("[{section}]"), new_line.to_string()],
            );
        } else {
            // 最近祖先段（逐级剥离尾段）。
            let mut ancestor_span = None;
            let mut probe = section.to_string();
            while let Some(idx) = probe.rfind('.') {
                probe.truncate(idx);
                if let Some(span) = locate_section(&lines, &probe) {
                    ancestor_span = Some(span);
                    break;
                }
            }
            if let Some((_, end)) = ancestor_span {
                insert_block(
                    &mut lines,
                    end,
                    vec![format!("[{section}]"), new_line.to_string()],
                );
            } else {
                let len = lines.len();
                insert_block(
                    &mut lines,
                    len,
                    vec![format!("[{section}]"), new_line.to_string()],
                );
            }
        }
    }

    let mut out = lines.join("\n");
    if crlf {
        out = out.replace('\n', "\r\n");
    }
    Ok(out)
}

// ── 读取 ───────────────────────────────────────────────────────────────────

fn read_root(config_path: &Path) -> Result<toml::Table> {
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("读取配置 {} 失败", config_path.display()))?;
    text.parse::<toml::Table>()
        .with_context(|| format!("解析配置 {} 失败（TOML 语法错误）", config_path.display()))
}

fn get_str<'a>(root: &'a toml::Table, path: &[&str]) -> Option<&'a str> {
    let mut node = root;
    for (i, key) in path.iter().enumerate() {
        let value = node.get(*key)?;
        if i + 1 == path.len() {
            return value.as_str();
        }
        node = value.as_table()?;
    }
    None
}

/// 配置中出现的 QQ 实例 id（有序去重）。

// ── 子命令实现 ─────────────────────────────────────────────────────────────

/// 打印端口清单（键 = 可直接用于 `port set` 的目标）。
fn list(config_path: &Path) -> Result<()> {
    let root = read_root(config_path)?;
    let mut rows: Vec<(String, String, &'static str)> = Vec::new();

    // 联邦监听布局（与运行期 `management::plan_federation_listen` 同口径：
    // 缺省合并 / 显式独立 / 纯连出 / 缺省回退；见 federation-port-merge.md）。
    let ListenDisplay {
        merged,
        effective,
        fallback,
    } = resolve_listen_display(&root);
    let listen_explicit = get_str(&root, &["federation", "listen"]).is_some();

    // management
    let management = get_str(&root, &["core", "management_address"]).unwrap_or_default();
    rows.push(if management.trim().is_empty() {
        (
            "management".into(),
            DEFAULT_MANAGEMENT.to_string(),
            if merged { "缺省·合并" } else { "缺省" },
        )
    } else {
        (
            "management".into(),
            management.trim().to_string(),
            if merged { "配置·合并" } else { "配置" },
        )
    });

    // federation listen / advertise
    match effective {
        None => rows.push(("federation".into(), "\"\"（仅连出）".into(), "配置")),
        Some(addr) => {
            let tag = if merged {
                if listen_explicit {
                    "配置·合并"
                } else {
                    "缺省·合并"
                }
            } else if listen_explicit {
                "配置"
            } else {
                "缺省"
            };
            rows.push(("federation".into(), addr, tag));
        }
    }
    match get_str(&root, &["federation", "advertise"]) {
        Some(v) if !v.trim().is_empty() => {
            rows.push(("federation-advertise".into(), v.trim().to_string(), "配置"))
        }
        _ => rows.push((
            "federation-advertise".into(),
            "（按 listen 推导）".into(),
            "未设置",
        )),
    }

    let width = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
    println!("端口清单（{}）", config_path.display());
    for (key, value, source) in &rows {
        let key_pad = width.saturating_sub(key.chars().count());
        let value_pad = 26usize.saturating_sub(display_width(value));
        println!(
            "  {key}{}  {value}{} [{source}]",
            " ".repeat(key_pad),
            " ".repeat(value_pad)
        );
    }
    println!();
    if merged {
        println!(
            "注: 端口合并已启用——联邦不独立监听，与管理面共用同一端口按令牌分流；\
             联邦入站随管理面插件启停"
        );
    } else if let Some(reason) = fallback {
        println!("注: 联邦缺省合并已回退为独立监听：{reason}");
    }
    println!("修改: echo-agent-core port set <键> <值>（复制第一列作为键）");
    println!("提示: 变更写回配置文件后，重启 echo-agent-core 生效");
    Ok(())
}

/// 列表展示的联邦监听布局（字段含义同 `management::plan_federation_listen`
/// 返回值；经同名规划函数计算，保证与运行期零口径差）。
struct ListenDisplay {
    merged: bool,
    effective: Option<String>,
    fallback: Option<&'static str>,
}

fn resolve_listen_display(root: &toml::Table) -> ListenDisplay {
    let raw_management = get_str(root, &["core", "management_address"]).unwrap_or_default();
    let management = if raw_management.trim().is_empty() {
        DEFAULT_MANAGEMENT.to_string()
    } else {
        raw_management.trim().to_string()
    };
    let token = get_str(root, &["core", "management_access_token"]).unwrap_or_default();
    let listen_raw = get_str(root, &["federation", "listen"]);
    let listen_explicit = listen_raw.is_some();
    let federation = listen_raw
        .map(str::to_string)
        .unwrap_or_else(|| DEFAULT_FEDERATION_LISTEN.to_string());
    // peer 令牌集合（[federation.peers.<name>].token；供缺省合并的冲突
    // 回退判定——与运行期 `fed.has_peer_token_sync` 同口径）。
    let peer_tokens: Vec<String> = root
        .get("federation")
        .and_then(|f| f.get("peers"))
        .and_then(|p| p.as_table())
        .map(|peers| {
            peers
                .values()
                .filter_map(|v| v.get("token").and_then(|t| t.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let (merged, effective, fallback) = crate::management::plan_federation_listen(
        &management,
        token, // get_str 返回 &str；直接传（&token 是 &&str，clippy 1.99 needless_borrow）
        &federation,
        listen_explicit,
        |t| peer_tokens.iter().any(|known| known == t),
    );
    ListenDisplay {
        merged,
        effective,
        fallback,
    }
}

/// 执行修改并写回。
fn set(config_path: &Path, target: &str, value: &str) -> Result<()> {
    let target = Target::parse(target).map_err(anyhow::Error::msg)?;
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("读取配置 {} 失败", config_path.display()))?;
    let root = text
        .parse::<toml::Table>()
        .with_context(|| format!("解析配置 {} 失败（TOML 语法错误）", config_path.display()))?;

    let (section, key) = target.section_key();

    // 仅当键**已存在**且值相等才算「未变化」（缺省 ≠ 显式空串/缺省值：
    // 如 federation.listen 缺省 = 监听 3133、显式空串 = 仅连出，语义不同，
    // 必须写显式值）。
    let unchanged = |current: Option<&str>, new: &str| -> bool {
        current.map(|c| c.trim() == new).unwrap_or(false)
    };

    enum Plan {
        StrValue(String),
        NoChange,
    }
    let plan = match &target {
        Target::Management => {
            let current = get_str(&root, &["core", "management_address"]);
            let new = normalize_addr(value, current.unwrap_or_default(), false)
                .map_err(anyhow::Error::msg)?;
            if unchanged(current, &new) {
                Plan::NoChange
            } else {
                Plan::StrValue(new)
            }
        }
        Target::FederationListen => {
            let current = get_str(&root, &["federation", "listen"]);
            let new = normalize_addr(value, current.unwrap_or_default(), true)
                .map_err(anyhow::Error::msg)?;
            if unchanged(current, &new) {
                Plan::NoChange
            } else {
                Plan::StrValue(new)
            }
        }
        Target::FederationAdvertise => {
            let current = get_str(&root, &["federation", "advertise"]);
            let new = normalize_advertise(value, current.unwrap_or_default())
                .map_err(anyhow::Error::msg)?;
            if unchanged(current, &new) {
                Plan::NoChange
            } else {
                Plan::StrValue(new)
            }
        }
    };

    /// 自检期望值（结构化——避免从展示文案反解类型）。
    enum Expect {
        Str(String),
    }
    let (updated, new_value_display, expect) = match plan {
        Plan::NoChange => {
            println!("未变化：{} （原值）", target_label(&target));
            return Ok(());
        }
        Plan::StrValue(new) => {
            let updated =
                set_string_key(&text, &section, &key, &new).map_err(anyhow::Error::msg)?;
            let expect = Expect::Str(new.clone());
            (updated, display_str(&new, &target), expect)
        }
    };

    // 自检：编辑后的文本必须仍是合法 TOML，且**目标值可按正确类型复读**
    //（防止字符串/整数形态写错——那会在 Core 启动加载配置时才暴露）。
    {
        let parsed: toml::Table = updated
            .parse()
            .map_err(|e| anyhow::anyhow!("内部错误：编辑后 TOML 解析失败（已放弃写入）: {e}"))?;
        let found = match (&target, &expect) {
            (Target::Management, _) => {
                get_str(&parsed, &["core", "management_address"]).map(str::to_string)
            }
            (Target::FederationListen, _) => {
                get_str(&parsed, &["federation", "listen"]).map(str::to_string)
            }
            (Target::FederationAdvertise, _) => {
                get_str(&parsed, &["federation", "advertise"]).map(str::to_string)
            }
        };
        let ok = match (&expect, &found) {
            (Expect::Str(expected), Some(found)) => found == expected,
            _ => false,
        };
        if !ok {
            return Err(anyhow::anyhow!(
                "内部错误：编辑后目标值复读不一致（已放弃写入；found={found:?}）"
            ));
        }
    }

    // tmp + rename 原子写（与 token 子命令同约定）。
    let tmp = config_path.with_extension("toml.port-tmp");
    std::fs::write(&tmp, &updated).with_context(|| format!("写入 {} 失败", tmp.display()))?;
    std::fs::rename(&tmp, config_path)
        .with_context(|| format!("替换 {} 失败", config_path.display()))?;

    println!("已更新 {} → {}", target_label(&target), new_value_display);
    println!("写回 {}（注释与其他段保留）", config_path.display());
    // 合并状态按**编辑后**的配置判定（用户意图的终态）：改 management 时
    // 若与 federation.listen 同址（或 listen 缺省跟随），联邦入站端口随
    // 之变化——提示里要提醒对端同步（2026-10 缺省合并语义）。
    let merged_after = updated
        .parse::<toml::Table>()
        .map(|t| resolve_listen_display(&t).merged)
        .unwrap_or(false);
    for hint in hints(&target, &new_value_display, merged_after) {
        println!("提示: {hint}");
    }
    println!("提示: 重启 echo-agent-core 生效（systemctl --user restart echo-agent-core）");
    Ok(())
}

fn target_label(target: &Target) -> String {
    match target {
        Target::Management => "management（[core].management_address）".into(),
        Target::FederationListen => "federation（[federation].listen）".into(),
        Target::FederationAdvertise => "federation-advertise（[federation].advertise）".into(),
    }
}

/// 终端显示宽度（ASCII = 1；CJK 等 = 2）——用于列表对齐。
fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| if (c as u32) < 0x1100 { 1 } else { 2 })
        .sum()
}

/// 字符串类目标的值展示：空串显示为 `""`（统一可读）。
fn display_str(value: &str, _target: &Target) -> String {
    if value.is_empty() {
        "\"\"".to_string()
    } else {
        value.to_string()
    }
}

fn hints(target: &Target, value: &str, merged: bool) -> Vec<String> {
    match target {
        Target::Management => {
            let port = split_host_port(value).map(|(_, p)| p).unwrap_or(0);
            let mut out = vec![format!(
                "Panel 端 [[cores]].url 需同步指向新端口（ws://<本机可达地址>:{port}）"
            )];
            if merged {
                // 缺省合并（或显式同址）：管理端口 = 联邦入站端口，对端
                // 拨号地址一并变化。
                out.push(format!(
                    "联邦与本端共址（端口合并）：对端 [federation.peers.*] 的 url 若指向本机也需同步（ws://<本机地址>:{port}）"
                ));
                out.push(
                    "端口映射（NAT）：外网映射指向的新端口与 [federation].advertise 一并核对"
                        .to_string(),
                );
            }
            out
        }
        Target::FederationListen => vec![
            "对端 peers 的 url 若指向本机端口需同步更新".to_string(),
            "本机 [federation].advertise 含端口时也需核对".to_string(),
            "独立监听为显式配置的逃生舱口；删除 listen 键（或与 management 同址）= 端口合并"
                .to_string(),
        ],
        Target::FederationAdvertise => {
            vec!["邀请串与对端学习的地址将按新值对外宣告".to_string()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "echo-port-cmd-test-{}-{tag}.toml",
            std::process::id()
        ));
        std::fs::remove_file(&p).ok();
        p
    }

    // ── 目标解析 ──

    #[test]
    fn target_parse_known_forms() {
        assert_eq!(Target::parse("management").unwrap(), Target::Management);
        assert_eq!(
            Target::parse("federation").unwrap(),
            Target::FederationListen
        );
        assert_eq!(
            Target::parse("federation-advertise").unwrap(),
            Target::FederationAdvertise
        );
        assert!(Target::parse("unknown").is_err());
        // 实例端口（QQ/NapCat 的部署形状）已移交插件：容器编排目标不存在了。
        assert!(Target::parse("qq").is_err());
        assert!(Target::parse("qq-instance:alix-two:webui").is_err());
    }

    // ── 值解析 ──

    #[test]
    fn normalize_addr_forms() {
        // 纯端口：沿用现有 host。
        assert_eq!(
            normalize_addr("4000", "127.0.0.1:3132", false).unwrap(),
            "127.0.0.1:4000"
        );
        // 无现值时用 0.0.0.0。
        assert_eq!(normalize_addr("4000", "", false).unwrap(), "0.0.0.0:4000");
        // 完整地址。
        assert_eq!(
            normalize_addr("10.0.0.5:5000", "127.0.0.1:3132", false).unwrap(),
            "10.0.0.5:5000"
        );
        // IPv6。
        assert_eq!(
            normalize_addr("[::1]:4000", "", false).unwrap(),
            "[::1]:4000"
        );
        // none 语义。
        assert_eq!(normalize_addr("none", "0.0.0.0:3133", true).unwrap(), "");
        assert!(normalize_addr("none", "x", false).is_err());
        // 非法。
        assert!(normalize_addr("0", "x:1", false).is_err());
        assert!(normalize_addr("70000", "x:1", false).is_err());
        assert!(normalize_addr("abc", "x:1", false).is_err());
        assert!(normalize_addr("host:bad", "x:1", false).is_err());
    }

    #[test]
    fn normalize_advertise_forms() {
        assert_eq!(
            normalize_advertise("10.0.0.5:4000", "").unwrap(),
            "10.0.0.5:4000"
        );
        // 纯 host（端口省略 → 采用 listen 端口）。
        assert_eq!(
            normalize_advertise("example.com", "").unwrap(),
            "example.com"
        );
        // 纯端口：沿用现有 host。
        assert_eq!(
            normalize_advertise("4000", "10.0.0.5:3133").unwrap(),
            "10.0.0.5:4000"
        );
        assert_eq!(normalize_advertise("none", "10.0.0.5:3133").unwrap(), "");
        // 无 host 且给纯端口 → 报错。
        assert!(normalize_advertise("4000", "").is_err());
        assert!(normalize_advertise("a b", "").is_err());
    }

    // ── 文本编辑 ──

    #[test]
    fn replaces_existing_key_preserving_everything() {
        let text = "# 头部注释\n[core]\nmanagement_address = \"0.0.0.0:3132\"\n# 尾部注释\n\n[federation]\nlisten = \"0.0.0.0:3133\"\n";
        let out = set_string_key(text, "core", "management_address", "127.0.0.1:4000").unwrap();
        assert!(out.contains("management_address = \"127.0.0.1:4000\""));
        assert!(out.contains("# 头部注释"));
        assert!(out.contains("# 尾部注释"));
        assert!(out.contains("listen = \"0.0.0.0:3133\""));
    }

    #[test]
    fn inserts_key_at_section_end() {
        let text = "[federation]\nadvertise = \"10.0.0.5:3133\"\n\n[federation.peers.a]\nurl = \"ws://x:1\"\n";
        let out = set_string_key(text, "federation", "listen", "0.0.0.0:4001").unwrap();
        let lines: Vec<&str> = out.lines().collect();
        let listen_idx = lines
            .iter()
            .position(|l| l.starts_with("listen = "))
            .expect("listen 已插入");
        // 插在 [federation] 段尾：在 peers 子表之前、advertise 之后。
        assert!(
            listen_idx
                > lines
                    .iter()
                    .position(|l| l.starts_with("advertise"))
                    .unwrap()
        );
        assert!(
            listen_idx
                < lines
                    .iter()
                    .position(|l| l.starts_with("[federation.peers"))
                    .unwrap()
        );
        // 仍然是合法 TOML 且值生效。
        let parsed: toml::Table = out.parse().unwrap();
        assert_eq!(
            parsed["federation"]["listen"].as_str().unwrap(),
            "0.0.0.0:4001"
        );
    }

    #[test]
    fn creates_section_before_descendant() {
        let text = "[federation.peers.a]\nurl = \"ws://x:1\"\n";
        let out = set_string_key(text, "federation", "listen", "0.0.0.0:4001").unwrap();
        // 新段插在子表之前，父表后置定义合法。
        let fed_idx = out.find("[federation]\n").unwrap();
        let peers_idx = out.find("[federation.peers.a]").unwrap();
        assert!(fed_idx < peers_idx);
        let parsed: toml::Table = out.parse().unwrap();
        assert_eq!(
            parsed["federation"]["listen"].as_str().unwrap(),
            "0.0.0.0:4001"
        );
    }

    #[test]
    fn creates_section_at_eof_when_missing() {
        let text = "[logging]\nlevel = \"info\"\n";
        let out = set_string_key(text, "core", "management_address", "127.0.0.1:4000").unwrap();
        let parsed: toml::Table = out.parse().unwrap();
        assert_eq!(
            parsed["core"]["management_address"].as_str().unwrap(),
            "127.0.0.1:4000"
        );
        assert_eq!(parsed["logging"]["level"].as_str().unwrap(), "info");
    }

    #[test]
    fn commented_key_is_not_matched() {
        let text = "[federation]\n# listen = \"0.0.0.0:3133\"\nadvertise = \"\"\n";
        let out = set_string_key(text, "federation", "listen", "0.0.0.0:4001").unwrap();
        // 注释原样保留，新的生效行插入。
        assert!(out.contains("# listen = \"0.0.0.0:3133\""));
        let parsed: toml::Table = out.parse().unwrap();
        assert_eq!(
            parsed["federation"]["listen"].as_str().unwrap(),
            "0.0.0.0:4001"
        );
    }

    #[test]
    fn prefix_key_is_not_matched() {
        let text = "[federation]\nlisten_extra = \"x\"\n";
        let out = set_string_key(text, "federation", "listen", "0.0.0.0:4001").unwrap();
        assert!(out.contains("listen_extra = \"x\""));
        let parsed: toml::Table = out.parse().unwrap();
        assert_eq!(
            parsed["federation"]["listen"].as_str().unwrap(),
            "0.0.0.0:4001"
        );
    }

    #[test]
    fn quoted_section_header_is_matched() {
        let text = "[adapters.qq.instances.\"qq\"]\npersona = \"alix\"\n";
        let out = set_string_key(text, "adapters.qq.instances.qq", "enabled", "true").unwrap();
        // 值写为字符串（本工具只写字符串；此用例仅验证段匹配）。
        let parsed: toml::Table = out.parse().unwrap();
        assert_eq!(
            parsed["adapters"]["qq"]["instances"]["qq"]["enabled"]
                .as_str()
                .unwrap(),
            "true"
        );
    }

    #[test]
    fn indented_key_keeps_indent() {
        let text = "[core]\n    management_address = \"0.0.0.0:3132\"\n";
        let out = set_string_key(text, "core", "management_address", "0.0.0.0:4000").unwrap();
        assert!(out.contains("    management_address = \"0.0.0.0:4000\""));
    }

    // ── 端到端（写文件 + 复读）──

    #[test]
    fn set_management_end_to_end() {
        let path = temp_path("e2e-mgmt");
        std::fs::write(
            &path,
            "# 配置\n[core]\nmanagement_address = \"127.0.0.1:3132\"\nmanagement_access_token = \"\"\n\n[federation]\nadvertise = \"10.0.0.5:3133\"\n",
        )
        .unwrap();
        // 先列一遍（不 panic 即视作通过）。
        list(&path).unwrap();
        set(&path, "management", "4000").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Table = text.parse().unwrap();
        assert_eq!(
            parsed["core"]["management_address"].as_str().unwrap(),
            "127.0.0.1:4000"
        );
        assert!(text.contains("# 配置"));
        // 相同值 → 未变化短路（文件保持原样）。
        let before = std::fs::read_to_string(&path).unwrap();
        set(&path, "management", "127.0.0.1:4000").unwrap();
        assert_eq!(before, std::fs::read_to_string(&path).unwrap());
        // 类型级验证：编辑后的配置能被 CoreConfig 真正加载（不只 TOML 合法）。
        let cfg = crate::config::CoreConfig::load(&path).unwrap();
        assert_eq!(cfg.core.management_address, "127.0.0.1:4000");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn set_federation_none() {
        let path = temp_path("e2e-fed");
        std::fs::write(&path, "[federation]\n").unwrap();
        set(&path, "federation", "none").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Table = text.parse().unwrap();
        assert_eq!(parsed["federation"]["listen"].as_str().unwrap(), "");
        // 类型级验证：编辑后的配置能被 CoreConfig 真正加载。
        let cfg = crate::config::CoreConfig::load(&path).unwrap();
        assert_eq!(cfg.federation.listen, "");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn e2e_list_contains_all_defaults() {
        let path = temp_path("e2e-list");
        std::fs::write(&path, "[logging]\nlevel = \"info\"\n").unwrap();
        let root = read_root(&path).unwrap();
        assert!(get_str(&root, &["core", "management_address"]).is_none());
        // 列表函数手跑不 panic。
        list(&path).unwrap();
        std::fs::remove_file(&path).ok();
    }

    /// 变更提示（`port set`）：合并态下改 management 要提醒对端联邦
    /// 拨号地址与 NAT 映射同步；独立监听时不出现联邦句（2026-10）。
    #[test]
    fn set_hints_cover_merged_federation() {
        let merged_hints = hints(&Target::Management, "0.0.0.0:8800", true);
        assert!(merged_hints.iter().any(|h| h.contains("Panel 端")));
        assert!(
            merged_hints
                .iter()
                .any(|h| h.contains("对端 [federation.peers.*]")),
            "合并态需提醒对端联邦地址同步: {merged_hints:?}"
        );
        assert!(merged_hints.iter().any(|h| h.contains("端口映射")));

        let standalone_hints = hints(&Target::Management, "0.0.0.0:8800", false);
        assert!(
            !standalone_hints.iter().any(|h| h.contains("对端")),
            "独立模式不应出现联邦同步提醒: {standalone_hints:?}"
        );

        // federation 显式配置的逃生舱口说明。
        let fed_hints = hints(&Target::FederationListen, "0.0.0.0:3133", false);
        assert!(fed_hints.iter().any(|h| h.contains("端口合并")));
    }

    /// 联邦监听布局（列表展示，与运行期同口径）。覆盖「缺省合并」语义的
    /// 全部分支：缺省+令牌→合并；缺省+空令牌→回退；缺省+回环→独立；
    /// 显式同址→合并；显式独立；显式空串→纯连出。
    #[test]
    fn listen_display_boundaries() {
        let parse = |s: &str| s.parse::<toml::Table>().unwrap();

        // ① 缺省 + 非回环 + 令牌 → 缺省合并（联邦跟随管理端口）。
        let d = resolve_listen_display(&parse(
            "[core]\nmanagement_address = \"0.0.0.0:3132\"\n\
             management_access_token = \"tok\"\n",
        ));
        assert!(d.merged, "缺省 + 令牌应为合并");
        assert_eq!(d.effective.as_deref(), Some("0.0.0.0:3132"));
        assert!(d.fallback.is_none());

        // ② 缺省 + 非回环 + 空令牌 → 回退独立监听（保守），带原因。
        let d = resolve_listen_display(&parse("[core]\nmanagement_address = \"0.0.0.0:3132\"\n"));
        assert!(!d.merged);
        assert_eq!(d.effective.as_deref(), Some(DEFAULT_FEDERATION_LISTEN));
        assert!(d.fallback.is_some());

        // ③ 缺省 + 回环管理地址 → 独立监听（合并会让联邦不可被连入）。
        let d = resolve_listen_display(&parse(
            "[core]\nmanagement_address = \"127.0.0.1:3132\"\n\
             management_access_token = \"tok\"\n",
        ));
        assert!(!d.merged);
        assert_eq!(d.effective.as_deref(), Some(DEFAULT_FEDERATION_LISTEN));
        assert!(d.fallback.is_none(), "回环是正常不合并而非回退");

        // ④ 显式同址 → 合并。
        let d = resolve_listen_display(&parse(
            "[core]\nmanagement_address = \"0.0.0.0:3132\"\n\
             [federation]\nlisten = \"0.0.0.0:3132\"\n",
        ));
        assert!(d.merged);
        assert_eq!(d.effective.as_deref(), Some("0.0.0.0:3132"));

        // ⑤ 显式独立（含 host 不同不合并——精确匹配）。
        let d = resolve_listen_display(&parse(
            "[core]\nmanagement_address = \"127.0.0.1:9000\"\n\
             [federation]\nlisten = \"0.0.0.0:9000\"\n",
        ));
        assert!(!d.merged);
        assert_eq!(d.effective.as_deref(), Some("0.0.0.0:9000"));

        // ⑥ 显式空串 → 纯连出（不监听）。
        let d = resolve_listen_display(&parse(
            "[core]\nmanagement_address = \"0.0.0.0:3132\"\n\
             [federation]\nlisten = \"\"\n",
        ));
        assert!(!d.merged);
        assert!(d.effective.is_none());
    }
}
