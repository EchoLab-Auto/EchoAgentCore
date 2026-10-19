//! Phase 3 门禁：**插件/机制 crate 不得依赖业务 crate**。
//!
//! 背景：解耦计划 Phase 3（见 `document/design/decoupling-plan.md`）把内置
//! 能力逐个外迁为独立 crate。外迁只有在「依赖方向真的收敛」时才有意义——
//! 一个 `echo-plugin-*` 如果又反向依赖 `echo-agent`，就等于把业务 crate
//! 换了个名字继续耦合。本测试把这个方向锁死为可断言的**只减不增**棘轮。
//!
//! 口径：
//! - 扫描各**机制层 / 插件层** crate 的 `Cargo.toml`，取 `[dependencies]`
//!   与 `[dev-dependencies]` 里的 echo-* 依赖名；
//! - 业务层（`echo-agent`）与组合根（`echo-agent-core`）永远不许出现；
//! - 每行左 = crate 名（目录相对仓库根）；右 = 允许的 echo-* 依赖白名单。
//!
//! 新增依赖必须同时改这张表——想在插件里用框架能力，应该走服务键 /
//! `HostCall`，而不是 `use echo_agent::…`。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// 业务 crate：任何机制层/插件层 crate 都不得依赖它们。
const BUSINESS_CRATES: [&str; 2] = ["echo-agent", "echo-agent-core"];

/// 允许的依赖表（crate 目录 → 允许的 echo-* 依赖）。
///
/// 只允许**向下**（定义层 `echo-defs` / 机制层 `echo-context` /
/// `echo-tool` / `echo-plugin*` / provider 实现）。
const ALLOWED: [(&str, &[&str]); 8] = [
    // 机制底座
    ("source/context/echo-context", &[]),
    ("source/defs/echo-defs", &[]),
    // dev-deps 里用 echo-sanitize 构造脱敏器（机制层安全 crate，非业务）
    (
        "source/tool/echo-tool",
        &["echo-defs", "echo-context", "echo-sanitize"],
    ),
    // 插件协议与宿主（宿主不得依赖 backend 层）
    // dev-deps 用 echo-defs 构造测试工具（定义层，非业务）
    ("source/plugin/echo-plugin", &["echo-context", "echo-defs"]),
    ("source/plugin/echo-plugin-api", &[]),
    ("source/plugin/echo-plugin-loader", &[]),
    (
        "source/plugin/echo-plugin-host",
        &["echo-defs", "echo-plugin-api", "echo-plugin-loader"],
    ),
    // Phase 3 外迁的内置插件
    (
        "source/plugin/echo-plugin-provider-llm",
        &[
            "echo-defs",
            "echo-llm-openai",
            "echo-llm-anthropic",
            "echo-llm-ollama",
        ],
    ),
];

/// 额外允许列表（后加入的外迁插件在此登记，避免改动上面的定长表）。
const ALLOWED_EXTRA: [(&str, &[&str]); 5] = [
    ("source/plugin/echo-plugin-skills", &["echo-defs"]),
    (
        "source/plugin/echo-plugin-loop",
        &["echo-defs", "echo-plugin", "echo-plugin-api"],
    ),
    // subagent 插件只经 `RemoteDelegationSink` 与宿主接缝（Phase 3 ⑥）
    (
        "source/plugin/echo-plugin-subagent",
        &["echo-defs", "echo-tool"],
    ),
    // management 插件只认线契约（Phase 3 ⑧）：echo-protocol + 联邦入站接收
    (
        "source/plugin/echo-plugin-management",
        &["echo-protocol", "echo-federation"],
    ),
    // workspace 插件只经 `RemoteWorkspaceQuery` 与宿主接缝（Phase 3 ⑤）；
    // echo-protocol 提供 `WorkspaceSessionInfo` 等**线格式词汇**（不是业务 crate）
    (
        "source/plugin/echo-plugin-workspace",
        &["echo-defs", "echo-tool", "echo-protocol", "echo-session"],
    ),
];

fn repo_root() -> PathBuf {
    // 本文件位于 source/core/tests/ → source/core → 仓库根
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

/// 从 Cargo.toml 取 echo-* 依赖名（`[dependencies]` / `[dev-dependencies]`）。
///
/// 手写解析：本仓库不引 TOML 解析器只为一个门禁测试；格式简单固定
/// （`name = { … }` / `name = "1"` / `name.workspace = true`）。
fn echo_deps(manifest: &Path) -> BTreeSet<String> {
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return BTreeSet::new();
    };
    let mut in_deps = false;
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_deps = line == "[dependencies]" || line == "[dev-dependencies]";
            continue;
        }
        if !in_deps || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, _rest)) = line.split_once('=') else {
            continue;
        };
        // `echo-defs.workspace = true` → key = "echo-defs.workspace"
        let name = key
            .trim()
            .split('.')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if name.starts_with("echo-") {
            out.insert(name);
        }
    }
    out
}

#[test]
fn plugin_and_mechanism_crates_never_depend_on_business_crates() {
    let root = repo_root();
    let mut violations = Vec::new();
    for (dir, _) in ALLOWED.iter().chain(ALLOWED_EXTRA.iter()) {
        let deps = echo_deps(&root.join(dir).join("Cargo.toml"));
        for business in BUSINESS_CRATES {
            if deps.contains(business) {
                violations.push(format!("{dir} → {business}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "机制层/插件层 crate 依赖了业务 crate（应改用服务键或 HostCall）：\n{}",
        violations.join("\n")
    );
}

#[test]
fn plugin_and_mechanism_crates_stay_within_the_allowed_dependency_table() {
    let root = repo_root();
    let mut violations = Vec::new();
    for (dir, allowed) in ALLOWED.iter().chain(ALLOWED_EXTRA.iter()) {
        let allowed: BTreeSet<&str> = allowed.iter().copied().collect();
        for dep in echo_deps(&root.join(dir).join("Cargo.toml")) {
            if !allowed.contains(dep.as_str()) {
                violations.push(format!("{dir} → {dep}（不在白名单）"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "出现未登记的 echo-* 依赖（确认方向正确后加进本测试的白名单表）：\n{}",
        violations.join("\n")
    );
}

#[test]
fn table_covers_every_candidate_crate() {
    let root = repo_root();
    let listed: BTreeSet<&str> = ALLOWED
        .iter()
        .chain(ALLOWED_EXTRA.iter())
        .map(|(dir, _)| *dir)
        .collect();
    // 新外迁的插件 crate 必须登记，否则门禁形同虚设。
    let mut missing = Vec::new();
    let plugin_dir = root.join("source/plugin");
    if let Ok(entries) = std::fs::read_dir(&plugin_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("echo-plugin") {
                continue;
            }
            let rel = format!("source/plugin/{name}");
            // sdk / example 不参与（插件侧运行时与示例，无 echo-* 业务依赖）
            if name == "echo-plugin-sdk" || name == "echo-plugin-example" {
                continue;
            }
            if !listed.contains(rel.as_str()) {
                missing.push(rel);
            }
        }
    }
    assert!(
        missing.is_empty(),
        "新插件 crate 未登记到依赖方向门禁：{}",
        missing.join(", ")
    );
}
