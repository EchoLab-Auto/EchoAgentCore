//! 文档新鲜度门禁（对齐 dsh 的「文档生成 + 新鲜度校验」）。
//!
//! 三类检查，都是**纯本地、无外部依赖**的：
//!
//! 1. **工具目录**（`document/plugins/tool-catalog.md`）：由内置工具注册表
//!    生成。加了工具忘了写文档 → 这里红；改了描述 → 这里也红（目录里存的就是
//!    描述原文）。
//! 2. **内置插件清单**：`BUILTIN_PLUGIN_IDS` 的每一项必须出现在
//!    `document/plugins/plugins.md` 的清单里（那个表是手写散文，不做
//!    自动生成，但**不许漏**）。
//! 3. **相对链接可达**：`document/` 下所有 `](相对路径)` 的目标文件必须存在
//!    ——重命名文档 / 拆分章节最容易留下死链，而读者点到 404 才知道。
//!
//! 刷新方式：`UPDATE_DOCS=1 cargo test -p echo-agent-core --test doc_catalog`
//! 会把生成类文件重写一遍（只改自动生成的部分，人工章节不受影响）。

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("source/")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn reading(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// 自动生成文件的首行标记：人类据此知道**不要手改**，测试据此知道哪段要覆盖。
const GENERATED_MARKER: &str = "<!-- 自动生成，请勿手改：";

/// 生成内置工具目录（稳定的行序：按名字排序，避免注册顺序抖动导致无谓 diff）。
async fn render_tool_catalog() -> String {
    let dir = std::env::temp_dir().join(format!("echo-doc-catalog-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let store = echo_config::ConfigStore::new(dir.join("core.toml"));
    let registry = echo_agent::tool::builtin::build_registry(
        std::sync::Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
        dir.clone(),
        store,
        None,
        echo_agent::shell::ShellHandle::new(),
        None,
        // Shell 工具（`bash-start` / `bash-exec` / `bash-stop`）由 shell 插件造
        // （2026-11 拆出）——目录要包含它们，所以这里给它一个 standalone provider。
        Some(echo_plugin_shell::standalone_provider()),
    );
    // `full_definitions` 是**面板同源**的那一份：name / description / category /
    // package 全在里面（`snapshot` 拿的是工具状态快照，不是定义）。
    let mut rows: Vec<(String, String, String)> = registry
        .full_definitions()
        .await
        .into_iter()
        .map(|(definition, category, _enabled, package)| {
            let description = summarize(&definition.description);
            let package = package.unwrap_or_default();
            (
                definition.name,
                category,
                format!("{description}｜包：`{package}`"),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let _ = std::fs::remove_dir_all(&dir);

    let mut out = String::new();
    // frontmatter 也在生成范围内：ProDoc 文档图靠它把本文件摆上画布，
    // 手写一份再让测试比对两处，等于凭空多一个漂移点。
    out.push_str("---\n");
    out.push_str("id: tool-catalog\n");
    out.push_str("title: \"内置工具目录\"\n");
    out.push_str("group: 插件\n");
    out.push_str("x: 960\n");
    out.push_str("y: 1780\n");
    out.push_str("---\n\n");
    out.push_str(&format!(
        "{GENERATED_MARKER}`echo-agent::tool::builtin::build_registry`） -->\n\n"
    ));
    out.push_str("# 内置工具目录\n\n");
    out.push_str("> 本文件由 `source/core/tests/doc_catalog.rs` 生成并校验。\n");
    out.push_str("> 刷新：`UPDATE_DOCS=1 cargo test -p echo-agent-core --test doc_catalog`。\n");
    out.push_str("> 机制说明见 [工具系统](./tools.md)；插件贡献的工具见 [插件化设计](./plugins.md)。\n\n");
    // 两个 H2：ProDoc 的悬停分块只认 H2，纯表格文件会没有分块面板。
    out.push_str("## 工具一览\n\n");
    out.push_str(&format!("共 **{}** 个内置工具，按注册名排序。\n\n", rows.len()));
    out.push_str("| 工具 | 分类 | 说明 |\n| --- | --- | --- |\n");
    for (name, category, description) in &rows {
        out.push_str(&format!("| `{name}` | {category} | {description} |\n"));
    }
    out.push_str("\n## 阅读约定\n\n");
    out.push_str("- **工具**：注册名，模型看到的就是它（`echo_defs::Tool::name`）。\n");
    out.push_str("- **分类**：面板「工具」页的分组依据（`category`），决定默认启用与否的粒度。\n");
    out.push_str("- **说明**：工具自述的**截断版**（超 120 字截）。完整用法看工具描述本身、`ToolsList` 事件或面板；\n");
    out.push_str("  每个工具的参数 schema 与调用语义归 [工具系统](./tools.md)。\n");
    out.push_str("- **包**：实现该工具的 crate / 插件（`package`），用来判断「这个工具是谁提供的」。\n\n");
    out.push_str("刷新与门禁见[文档规范](../documentation-guide.md)「自动生成的文件」：改注册表后跑 `UPDATE_DOCS=1`，\n");
    out.push_str("否则门禁比对失败并给出首个不同行。\n");
    out
}

/// 描述压成一行、超长截断：目录是**索引**，不是全文。
///
/// 工具描述里有整段用法说明（例如 `bash` 的 `node://` 前缀说明），原样搬进
/// 表格会把每行撑成三屏——读者反而找不到重点。完整描述从 `ToolsList`
/// 事件（面板）或工具的 `description()` 取。
fn summarize(description: &str) -> String {
    const MAX: usize = 120;
    let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX {
        return flat;
    }
    let truncated: String = flat.chars().take(MAX).collect();
    format!("{truncated}…")
}

/// 生成类文件与磁盘内容一致；`UPDATE_DOCS=1` 时改写而不是失败。
fn assert_fresh(relative: &str, generated: String) {
    let path = repo_root().join(relative);
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current == generated {
        return;
    }
    if std::env::var_os("UPDATE_DOCS").is_some() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        std::fs::write(&path, &generated).expect("write doc");
        eprintln!("已刷新 {}", path.display());
        return;
    }
    // 差异定位：给出首个不同行，省得人从头 diff 两百行。
    let first_diff = current
        .lines()
        .zip(generated.lines())
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map(|(i, (a, b))| format!("第 {} 行:\n  磁盘: {a}\n  生成: {b}", i + 1))
        .unwrap_or_else(|| {
            format!(
                "行数不同（磁盘 {} / 生成 {}）",
                current.lines().count(),
                generated.lines().count()
            )
        });
    panic!(
        "{} 已过期（{first_diff}）\n刷新：UPDATE_DOCS=1 cargo test -p echo-agent-core --test doc_catalog",
        path.display()
    );
}

/// 工具目录新鲜度：注册表 → `document/plugins/tool-catalog.md`。
#[tokio::test]
async fn tool_catalog_is_fresh() {
    assert_fresh(
        "document/plugins/tool-catalog.md",
        render_tool_catalog().await,
    );
}

/// 内置插件 id 必须出现在插件化设计文档的清单里。
///
/// 不做整表自动生成：那张表里「说明」一列是人写的高信号内容，自动生成只会
/// 把散文变成机器产物。这里守的是**不许漏**（新增插件忘了登记是最常见的漂移）。
#[test]
fn documented_builtin_plugin_ids_cover_the_constant() {
    let doc = reading(&repo_root().join("document/plugins/plugins.md"));
    let mut missing = Vec::new();
    for id in echo_agent::plugins::BUILTIN_PLUGIN_IDS {
        if !doc.contains(id) {
            missing.push(id);
        }
    }
    assert!(
        missing.is_empty(),
        "以下内置插件 id 未出现在 document/plugins/plugins.md：{missing:?}"
    );
}

/// `document/` 下所有相对链接的目标文件必须存在。
#[test]
fn document_relative_links_resolve() {
    let root = repo_root().join("document");
    let mut dead: Vec<String> = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let text = reading(&path);
            for target in relative_links(&text) {
                let resolved = path.parent().expect("parent").join(&target);
                if !resolved.exists() {
                    dead.push(format!(
                        "{} → {}",
                        path.strip_prefix(&root).unwrap_or(&path).display(),
                        target
                    ));
                }
            }
        }
    }
    dead.sort();
    assert!(
        dead.is_empty(),
        "文档存在 {} 条死链（目标文件不存在）：\n{}",
        dead.len(),
        dead.join("\n")
    );
}

/// 抠出所有 `](相对路径)` 的目标（跳过外链 / 纯锚点）。
fn relative_links(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b'(' {
            let start = i + 2;
            let Some(end_offset) = text[start..].find(')') else {
                break;
            };
            let raw = &text[start..start + end_offset];
            let target = raw.split('#').next().unwrap_or_default().trim();
            if !target.is_empty()
                && !target.starts_with("http")
                && !target.starts_with("mailto:")
                && !target.starts_with('<')
            {
                out.push(target.to_string());
            }
            i = start + end_offset;
            continue;
        }
        i += 1;
    }
    out
}
