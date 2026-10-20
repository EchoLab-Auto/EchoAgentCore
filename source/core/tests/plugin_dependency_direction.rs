//! 依赖方向门禁（Phase 3）：**机制层/插件层 crate 不得依赖业务 crate**。
//!
//! 白名单的单一真值来源是 `scripts/deps-policy.txt`——同一份文件也被 CI 的
//! `scripts/check-deps.sh` 读取。两者共读一张表，杜绝「CI 的清单和 cargo
//! test 的清单各说一套」：此前正是如此（shell 脚本硬编码 14 个 crate，
//! Phase 3 新增的 7 个一个都没查，脚本报 OK 而实际越界无人发现）。
//!
//! 本测试比 shell 脚本多两件事（脚本里同样实现了，这里做交叉验证）：
//! - 业务 crate `echo-agent` 只允许 `core`（组合根）依赖；
//! - `core` 之外一律不许出现 `echo-agent`，且任何 crate 不许依赖
//!   `echo-agent-core`（那是二进制名）。
//!
//! 口径：`[dependencies]` 与 `[dev-dependencies]` 都算（测试里引入业务 crate
//! 同样是方向违规——它会污染被外迁 crate 的编译依赖）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// 业务 crate：除组合根外，任何 crate 都不得依赖。
const BUSINESS_CRATE: &str = "echo-agent";
/// 组合根（唯一允许依赖业务 crate 的 crate 目录）。
const COMPOSITION_ROOT: &str = "core";
/// 二进制 crate 名：不应被任何 crate 依赖。
const BINARY_CRATE: &str = "echo-agent-core";

fn repo_root() -> PathBuf {
    // 本文件位于 source/core/tests/ → source/core → 仓库根
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

/// 读 `scripts/deps-policy.txt`：`crate 目录 → 允许的 echo-* 依赖集合`。
fn policy() -> BTreeMap<String, BTreeSet<String>> {
    let path = repo_root().join("scripts/deps-policy.txt");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读不到依赖白名单 {}: {e}", path.display()));
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(crate_dir) = parts.next() else {
            continue;
        };
        let allowed = parts.map(str::to_string).collect::<BTreeSet<_>>();
        let previous = out.insert(crate_dir.to_string(), allowed);
        assert!(
            previous.is_none(),
            "白名单里 {crate_dir} 出现了两次（后者会静默覆盖前者）"
        );
    }
    assert!(!out.is_empty(), "白名单为空——门禁形同虚设");
    out
}

/// 从 Cargo.toml 取 echo-* 依赖名（`[dependencies]` 与 `[dev-dependencies]`）。
///
/// 手写解析：门禁测试不该为了读 manifest 引入 TOML 依赖；格式固定
/// （`name = { … }` / `name.workspace = true` / `name = "1"`）。
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
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        // `echo-defs.workspace = true` → 取 `echo-defs`
        let name = key.trim().split('.').next().unwrap_or("").trim();
        if name.starts_with("echo-") {
            out.insert(name.to_string());
        }
    }
    out
}

/// `source/` 下所有含 Cargo.toml 的 crate 目录（相对 `source/`）。
fn discovered_crates() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let source = repo_root().join("source");
    for group in std::fs::read_dir(&source).into_iter().flatten().flatten() {
        if !group.path().is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(group.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let dir = entry.path();
            if dir.join("Cargo.toml").is_file() {
                let rel = dir
                    .strip_prefix(&source)
                    .expect("under source")
                    .to_string_lossy()
                    .to_string();
                out.insert(rel);
            }
        }
    }
    out
}

#[test]
fn every_crate_depends_only_on_what_the_policy_allows() {
    let root = repo_root();
    let mut violations = Vec::new();
    for (crate_dir, allowed) in policy() {
        let manifest = root.join("source").join(&crate_dir).join("Cargo.toml");
        assert!(
            manifest.is_file(),
            "白名单里的 {crate_dir} 不存在（{}）——删 crate 时记得同步 scripts/deps-policy.txt",
            manifest.display()
        );
        for dep in echo_deps(&manifest) {
            if !allowed.contains(&dep) {
                violations.push(format!("{crate_dir} → {dep}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "出现未登记的 echo-* 依赖（确认方向正确后加进 scripts/deps-policy.txt）：\n{}",
        violations.join("\n")
    );
}

#[test]
fn business_crate_is_only_reachable_from_the_composition_root() {
    let root = repo_root();
    let mut violations = Vec::new();
    for crate_dir in discovered_crates() {
        let manifest = root.join("source").join(&crate_dir).join("Cargo.toml");
        let deps = echo_deps(&manifest);
        if deps.contains(BINARY_CRATE) {
            violations.push(format!(
                "{crate_dir} → {BINARY_CRATE}（二进制名不可作为依赖）"
            ));
        }
        if deps.contains(BUSINESS_CRATE) && crate_dir != COMPOSITION_ROOT {
            violations.push(format!(
                "{crate_dir} → {BUSINESS_CRATE}（业务 crate 只允许 {COMPOSITION_ROOT} 依赖）"
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "业务 crate 被越界依赖（应改用服务键 / HostCall / trait 注入）：\n{}",
        violations.join("\n")
    );
}

#[test]
fn every_crate_is_registered_in_the_policy() {
    let declared = policy().into_keys().collect::<BTreeSet<_>>();
    let missing = discovered_crates()
        .into_iter()
        .filter(|c| !declared.contains(c))
        .collect::<Vec<_>>();
    // 新 crate（尤其新外迁的插件）不登记就等于门禁对它失效。
    assert!(
        missing.is_empty(),
        "以下 crate 未登记到 scripts/deps-policy.txt（门禁对它们失效）：\n{}",
        missing.join("\n")
    );
}

/// 白名单自身形状自检：定义层必须零依赖，插件层不出现业务 crate。
#[test]
fn policy_shape_matches_the_layering_contract() {
    let p = policy();
    // `context/echo-context` 已随框架外迁（2026-10-19 M3），本仓只剩这两个
    // 零 echo-* 依赖的底座 crate；框架侧的等价约束在
    // `plugin-framework/crates/framework-gates/tests/domain_agnostic.rs`。
    for zero in ["defs/echo-defs", "config/echo-config"] {
        let allowed = p.get(zero).unwrap_or_else(|| panic!("{zero} 未登记"));
        assert!(
            allowed.is_empty(),
            "{zero} 是底座 crate，不允许任何 echo-* 依赖，实际: {allowed:?}"
        );
    }
    for (crate_dir, allowed) in &p {
        if crate_dir.starts_with("plugin/") {
            assert!(
                !allowed.contains(BUSINESS_CRATE),
                "{crate_dir} 是插件层 crate，不允许依赖 {BUSINESS_CRATE}"
            );
        }
    }
}
