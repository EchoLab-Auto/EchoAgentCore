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

// ── 命令域实现的归属（S11）───────────────────────────────────────────────

/// 某条命令的**实现**不得留在组合根（否则"命令域归插件"只是声明）。
///
/// 背景：`echo-plugin-federation::commands` 一直声明处理那 5 条命令，但
/// 真正的处理在 `core/federation_rt.rs::handle_federation_command`（300 行）。
/// 于是"哪条命令归谁"读插件读得出来、读宿主读不出来——归属是假的。
///
/// 这类漂移只有源码能抓：两条路径都编译通过、测试也全绿，只是实现长在
/// 错的一边。所以直接读源码断言：**组合根里不得出现 `BackendCommand::<名>`
/// 的模式匹配臂**（模式匹配是"这里处理它"的唯一形状；注释不算）。
#[test]
fn plugin_command_implementations_do_not_live_in_the_composition_root() {
    let root = repo_root().join("source");

    // 扫描全部源码，收集每个 crate 目录里出现的 `BackendCommand::<X>` 匹配臂。
    let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    walk_rs(&root, &mut |path, text| {
        let Some((crate_name, relative)) = crate_of(&root, path) else {
            return;
        };
        for (index, line) in text.lines().enumerate() {
            let code = line.trim_start();
            // 注释与文档里的例子不算实现。
            if code.starts_with("//") || code.starts_with('*') {
                continue;
            }
            if let Some(pos) = line.find("BackendCommand::") {
                let rest = &line[pos + "BackendCommand::".len()..];
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if name.is_empty() {
                    continue;
                }
                owners
                    .entry(crate_name.clone())
                    .or_default()
                    .insert(format!("{name}@{}:{relative}", index + 1));
            }
        }
    });
    // 这 5 条由 `echo-plugin-federation` 处理；组合根只允许**分发**（
    // `apply_command` 里的兜底 match 不再提及它们，因为插件已认领）。
    let federation_commands = [
        "SaveFederationPeer",
        "DeleteFederationPeer",
        "RequestFederationStatus",
        "RequestFederationInvite",
        "MigrateSession",
    ];
    let core = owners.get("core").cloned().unwrap_or_default();
    let leaked: Vec<&String> = core
        .iter()
        .filter(|entry| {
            let name = entry.split('@').next().unwrap_or(entry);
            federation_commands.contains(&name)
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "这些联邦命令的实现仍在组合根里（应搬进 echo-plugin-federation::admin，\
         宿主只实现 FederationAdminHost 的原子动作）：{leaked:#?}"
    );

    // 反方向：插件必须**真的**认得它们（只删不搬 = 命令没人处理）。
    let plugin = owners
        .get("echo-plugin-federation")
        .cloned()
        .unwrap_or_default();
    let owned: BTreeSet<&str> = plugin
        .iter()
        .map(|entry| entry.split('@').next().unwrap_or(entry))
        .collect();
    for command in federation_commands {
        assert!(
            owned.contains(command),
            "插件没有处理 {command}（它只出现在扫描结果的这些地方：{owned:?}）——\
             命令会变成无人认领的死路"
        );
    }

    // 扫描器自身的守卫：它至少要看得到「别处确实有命令匹配」——否则上面的
    // 断言可能只是因为"一条都没扫到"而恒真。
    let total: usize = owners.values().map(BTreeSet::len).sum();
    assert!(
        total > federation_commands.len(),
        "扫描器失效（只找到 {total} 处 BackendCommand:: 匹配；路径规则改过？）"
    );
}

/// 路径 → `(crate 目录名, 相对路径)`；`target/` 与测试目录下的数据跳过。
fn crate_of(root: &Path, path: &Path) -> Option<(String, String)> {
    let relative = path.strip_prefix(root).ok()?;
    let mut parts = relative.components();
    // source/<组>/<crate>/...
    let _group = parts.next()?;
    let crate_name = parts.next()?.as_os_str().to_string_lossy().to_string();
    if crate_name == "plugin" {
        // `source/plugin/<crate>/…`：插件 crate 多一层。
        let inner = parts.next()?.as_os_str().to_string_lossy().to_string();
        return Some((inner, relative.display().to_string()));
    }
    Some((crate_name, relative.display().to_string()))
}

fn walk_rs(dir: &Path, f: &mut impl FnMut(&Path, &str)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            walk_rs(&path, f);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                f(&path, &text);
            }
        }
    }
}

// ── 插件之间互不依赖（S12）────────────────────────────────────────────────

/// 插件层**不得依赖别的插件**：只允许机制 crate。
///
/// 背景（2026-11 实测）：曾有 4 处插件互依赖，全部是"一个 trait/struct 长在了
/// 错的一边"——federation 要 `impl echo_plugin_subagent::RemoteDelegationSink`、
/// `use echo_plugin_workspace::{QueryFuture, RemoteWorkspaceQuery}`、
/// `type RemoteInvoker = echo_plugin_tools::…`；tools 要
/// `use echo_plugin_provider_llm::api_pool::PoolSnapshot`。
///
/// 后果不是"多一条边"，而是**替换不可能**：想换掉 federation 就必须先接受
/// subagent 的词汇。它们一直能编译、一直没人拦。
///
/// 例外只有机制 crate（"插件怎么装"，不是"某个插件"）：
/// `echo-plugin` / `echo-plugin-api` / `echo-plugin-host` / `echo-plugin-sdk`
/// / `echo-plugin-loader`。
///
/// 本条与 `scripts/deps-policy.txt` 的白名单**互为冗余**：policy 是人读的
/// 真值来源，这里给出可执行的诊断信息（点名是哪两个插件、该怎么修）。
#[test]
fn plugins_do_not_depend_on_other_plugins() {
    const MECHANISM: [&str; 5] = [
        "echo-plugin",
        "echo-plugin-api",
        "echo-plugin-host",
        "echo-plugin-sdk",
        "echo-plugin-loader",
    ];
    let root = repo_root().join("source/plugin");
    let mut violations = Vec::new();
    let mut plugins_seen = 0usize;
    for entry in std::fs::read_dir(&root).expect("source/plugin") {
        let dir = entry.expect("dir entry").path();
        let cargo = dir.join("Cargo.toml");
        if !cargo.exists() {
            continue;
        }
        let name = dir.file_name().unwrap_or_default().to_string_lossy().to_string();
        plugins_seen += 1;
        let text = std::fs::read_to_string(&cargo).expect("read Cargo.toml");
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((dep, _)) = line.split_once('=') else {
                continue;
            };
            let dep = dep.trim();
            // `echo-plugin-xxx` 但**不是**机制 crate → 插件互依赖。
            if dep.starts_with("echo-plugin-") && !MECHANISM.contains(&dep) {
                violations.push(format!(
                    "{name} → {dep}（应把共用的 trait/struct 下沉到 echo-defs，\
                     或改成服务键调用）"
                ));
            }
        }
    }
    assert!(plugins_seen >= 9, "只扫到 {plugins_seen} 个插件 crate（路径规则改过？）");
    assert!(
        violations.is_empty(),
        "插件之间不得互相依赖（S12）：\n{}\n\
         修法二选一：① 共用类型 → 下沉 `echo-defs`（跨边界词汇的家）；\
         ② 想调能力 → 服务键（`MountContext::require`）。",
        violations.join("\n")
    );
}
