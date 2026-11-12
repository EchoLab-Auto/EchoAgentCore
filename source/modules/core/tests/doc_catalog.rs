//! 文档新鲜度门禁（对齐 dsh 的「文档生成 + 新鲜度校验」）。
//!
//! 三类检查，都是**纯本地、无外部依赖**的：
//!
//! 1. **工具目录**（`document/modules/tool-catalog.md`）：由内置工具注册表
//!    生成。加了工具忘了写文档 → 这里红；改了描述 → 这里也红（目录里存的就是
//!    描述原文）。
//! 2. **内置模块清单**：`BUILTIN_MODULE_IDS` 的每一项必须出现在
//!    `document/modules/modules.md` 的清单里（那个表是手写散文，不做
//!    自动生成，但**不许漏**）。
//! 3. **相对链接可达**：`document/` 下所有 `](相对路径)` 的目标文件必须存在
//!    ——重命名文档 / 拆分章节最容易留下死链，而读者点到 404 才知道。
//! 4. **散文里的数字**：`modules.md` 声称的「框架内置 N 项」与 `[&str; N]`
//!    必须等于 `BUILTIN_MODULE_IDS.len()`。第 2 类检查只问「id 出现过没有」，
//!    于是"表里 13 行、正文写 10 项"能从门禁底下走过去（2026-11 实际发生过）。
//! 5. **非 document 目录的文档引用**：`config/*.toml` 与 `skills/**/SKILL.md`
//!    也会写 `document/xxx.md` 路径。第 3 类只扫 `document/` 自身，扫不到它们
//!    ——出厂配置模板指向一个不存在的文件名，用户点开就是 404。
//! 6. **模块命令名**：每个模块 `COMMANDS` 里的名字都得在文档里出现过。
//! 7. **模块树一比一**（`document/modules/modules.md`）：12 个内置模块 +
//!    实例模块 `echo-agent.instance.qq`，每一项都必须写成 `| [`id`](路径) |`
//!    形态、且那个路径真实存在。第 2 类只查「id 字符串出现过没有」——一行纯
//!    文本也算过，而画布上 `modules` 的子树正是从这些链接长出来的：漏一个
//!    链接，模块树就少一个节点，读者从树上走不到它的契约。
//!
//! 刷新方式：`UPDATE_DOCS=1 cargo test -p echo-agent-core --test doc_catalog`
//! 会把生成类文件重写一遍（只改自动生成的部分，人工章节不受影响）。

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("modules/")
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
    let registry = agent_module_loop::tool::builtin::build_registry(
        std::sync::Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
        dir.clone(),
        store,
        None,
        agent_module_loop::shell::ShellHandle::new(),
        Some(std::sync::Arc::new(
            agent_module_loop::agent::session_query::SessionQueryRegistry::new(),
        )),
        None,
        // Shell 工具（`bash-start` / `bash-exec` / `bash-stop`）由 shell 模块造
        // （2026-11 拆出）——目录要包含它们，所以这里给它一个 standalone provider。
        Some(agent_module_shell::standalone_provider()),
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
    out.push_str("group: Core\n");
    // `x` / `y` 归 `tools/prodoc_layout.py`（画布坐标的唯一写者），这里不写：
    // 两个写者争同一字段时，刷新一次就把布局改回去，再跑布局又变回来，永远
    // 在两者之间来回。`assert_fresh` 比对时也剥掉这两行。
    out.push_str("---\n\n");
    out.push_str(&format!(
        "{GENERATED_MARKER}`echo-agent::tool::builtin::build_registry`） -->\n\n"
    ));
    out.push_str("# 内置工具目录\n\n");
    out.push_str("> 本文件由 `source/modules/core/tests/doc_catalog.rs` 生成并校验。\n");
    out.push_str("> 刷新：`UPDATE_DOCS=1 cargo test -p echo-agent-core --test doc_catalog`。\n");
    out.push_str(
        "> 机制说明见 [工具系统](./tools.md)；模块贡献的工具见 [模块化设计](./modules.md)。\n\n",
    );
    // 两个 H2：ProDoc 的悬停分块只认 H2，纯表格文件会没有分块面板。
    out.push_str("## 工具一览\n\n");
    out.push_str(&format!(
        "共 **{}** 个内置工具，按注册名排序。\n\n",
        rows.len()
    ));
    out.push_str("| 工具 | 分类 | 说明 |\n| --- | --- | --- |\n");
    for (name, category, description) in &rows {
        out.push_str(&format!("| `{name}` | {category} | {description} |\n"));
    }
    out.push_str("\n## 阅读约定\n\n");
    out.push_str("- **工具**：注册名，模型看到的就是它（`echo_defs::Tool::name`）。\n");
    out.push_str("- **分类**：面板「工具」页的分组依据（`category`），决定默认启用与否的粒度。\n");
    out.push_str("- **说明**：工具自述的**截断版**（超 120 字截）。完整用法看工具描述本身、`ToolsList` 事件或面板；\n");
    out.push_str("  每个工具的参数 schema 与调用语义归 [工具系统](./tools.md)。\n");
    out.push_str(
        "- **包**：实现该工具的 crate / 模块（`package`），用来判断「这个工具是谁提供的」。\n\n",
    );
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
    // 画布坐标归 `tools/prodoc_layout.py`：生成器不产 `x` / `y`，磁盘上那份
    // 由布局脚本写入。比对时两侧都剥掉，否则每重排一次布局这条门禁就红一次。
    let strip_layout = |text: &str| {
        text.lines()
            .filter(|line| {
                let line = line.trim_start();
                !(line.starts_with("x: ") || line.starts_with("y: "))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (current_cmp, generated_cmp) = (strip_layout(&current), strip_layout(&generated));
    if current_cmp == generated_cmp {
        return;
    }
    if std::env::var_os("UPDATE_DOCS").is_some() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        std::fs::write(&path, &generated).expect("write doc");
        eprintln!("已刷新 {}", path.display());
        return;
    }
    // 差异定位：给出首个不同行，省得人从头 diff 两百行。
    let first_diff = current_cmp
        .lines()
        .zip(generated_cmp.lines())
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map(|(i, (a, b))| format!("第 {} 行:\n  磁盘: {a}\n  生成: {b}", i + 1))
        .unwrap_or_else(|| {
            format!(
                "行数不同（磁盘 {} / 生成 {}）",
                current_cmp.lines().count(),
                generated_cmp.lines().count()
            )
        });
    panic!(
        "{} 已过期（{first_diff}）\n刷新：UPDATE_DOCS=1 cargo test -p echo-agent-core --test doc_catalog",
        path.display()
    );
}

/// 工具目录新鲜度：注册表 → `document/modules/tool-catalog.md`。
#[tokio::test]
async fn tool_catalog_is_fresh() {
    assert_fresh(
        "document/modules/tool-catalog.md",
        render_tool_catalog().await,
    );
}

/// 内置模块 id 必须出现在模块化设计文档的清单里。
///
/// 不做整表自动生成：那张表里「说明」一列是人写的高信号内容，自动生成只会
/// 把散文变成机器产物。这里守的是**不许漏**（新增模块忘了登记是最常见的漂移）。
#[test]
fn documented_builtin_module_ids_cover_the_constant() {
    let doc = reading(&repo_root().join("document/modules/modules.md"));
    let mut missing = Vec::new();
    for id in agent_module_loop::modules::BUILTIN_MODULE_IDS {
        if !doc.contains(id) {
            missing.push(id);
        }
    }
    assert!(
        missing.is_empty(),
        "以下内置模块 id 未出现在 document/modules/modules.md：{missing:?}"
    );
}

/// 模块树一比一：每个模块 id 必须**带一个可达的文档链接**。
///
/// 与上一条的分工：上一条问「id 字符串在不在」，这条问「树节点对不对」。
/// 画布上 `modules` 的子节点完全由这些链接生成（`link` 与正文表格由
/// `tools/prodoc_layout.py` 一起维护），所以少一个链接 = 少一个节点，
/// 多一个链接 = 树上挂了一个不存在的模块。
#[test]
fn every_module_has_a_reachable_document_link() {
    let doc_path = repo_root().join("document/modules/modules.md");
    let doc = reading(&doc_path);
    let base = doc_path.parent().expect("document/modules");

    let mut ids: Vec<&str> = agent_module_loop::modules::BUILTIN_MODULE_IDS.to_vec();
    ids.push("echo-agent.instance.qq");

    let mut missing = Vec::new();
    let mut dangling = Vec::new();
    for id in ids {
        let needle = format!("| [`{id}`](");
        let Some(start) = doc.find(&needle) else {
            missing.push(id);
            continue;
        };
        let rest = &doc[start + needle.len()..];
        let Some(end) = rest.find(')') else {
            dangling.push(format!("{id}（链接未闭合）"));
            continue;
        };
        let relative = &rest[..end];
        if !base.join(relative).is_file() {
            dangling.push(format!("{id} → {relative}"));
        }
    }

    assert!(
        missing.is_empty(),
        "以下模块 id 在 document/modules/modules.md 的清单里没有文档链接（模块树会少节点）：{missing:?}"
    );
    assert!(
        dangling.is_empty(),
        "以下模块的文档链接不可达（模块树指向不存在的文件）：{dangling:?}"
    );
}

/// `modules.md` 里的**数字**必须与代码一致，而不只是"id 出现过"。
///
/// 上一条检查有个洞：它只问 `doc.contains(id)`，所以 13 个 id 全在表里时它绿，
/// 而正文那句「**框架内置 10 项**（`BUILTIN_MODULE_IDS: [&str; 10]`）」照样错
/// ——2026-11 审计实测就是这个形状（表 13 行、正文 10 项，同一节自相矛盾）。
/// 散文数字没有生成渠道，只能这样钉住。
#[test]
fn modules_doc_counts_match_the_code() {
    let doc = reading(&repo_root().join("document/modules/modules.md"));
    let expected = agent_module_loop::modules::BUILTIN_MODULE_IDS.len();

    let claimed = doc
        .split("框架内置 ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse::<usize>().ok())
        .expect("modules.md 必须写「框架内置 N 项」（数字紧跟其后）");
    assert_eq!(
        claimed, expected,
        "modules.md 声称框架内置 {claimed} 项，代码是 {expected} 项"
    );

    let literal = doc
        .split("BUILTIN_MODULE_IDS: [&str; ")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .and_then(|n| n.parse::<usize>().ok())
        .expect("modules.md 必须写出 `BUILTIN_MODULE_IDS: [&str; N]` 的字面长度");
    assert_eq!(
        literal, expected,
        "modules.md 写的是 [&str; {literal}]，代码是 [&str; {expected}]"
    );
}

/// 配置模板、技能文档与正文里的**裸** `document/xxx.md` 路径也必须可达。
///
/// `document_relative_links_resolve` 只看 `](相对路径)` 形态、且只遍历
/// `document/` 自己。于是三类真实引用从门禁底下走过去：
/// 出厂配置模板的注释（`config/echo-agent-core.toml` 曾指向
/// `document/core/core-logging.md`，实际文件名是 `logging.md`）、
/// 会下发到用户数据目录的 `skills/**/SKILL.md`、以及文档正文里写成反引号的
/// 裸路径（`document/develop/…` 这种本仓根本不存在的目录）。
#[test]
fn bare_document_paths_resolve() {
    let root = repo_root();
    let mut sources: Vec<PathBuf> = Vec::new();
    // `document/` 也要扫：正文裸路径与链接是两种写法。
    for dir in ["config", "skills", "document"] {
        let mut stack = vec![root.join(dir)];
        while let Some(current) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "toml" || e == "md") {
                    sources.push(path);
                }
            }
        }
    }

    let mut dead = Vec::new();
    for path in sources {
        for (line_no, line) in reading(&path).lines().enumerate() {
            for (start, _) in line.match_indices("document/") {
                // 外链里的片段（`…/blob/main/document/x.md`）是别仓路径。
                if line[..start].ends_with('/') {
                    continue;
                }
                // 跨仓路径写成 `<仓名>:document/…`，带前缀即声明"不在本仓"。
                if line[..start].ends_with(':') {
                    continue;
                }
                let target: String = line[start..]
                    .chars()
                    .take_while(|c| {
                        !c.is_whitespace() && !matches!(c, '`' | ')' | '（' | ',' | '。' | '；')
                    })
                    .collect();
                if !target.ends_with(".md") {
                    continue;
                }
                if !root.join(&target).exists() {
                    dead.push(format!(
                        "{}:{} → {}",
                        path.strip_prefix(&root).unwrap_or(&path).display(),
                        line_no + 1,
                        target
                    ));
                }
            }
        }
    }
    dead.sort();
    dead.dedup();
    assert!(
        dead.is_empty(),
        "有 {} 条指向不存在文档的引用：\n{}",
        dead.len(),
        dead.join("\n")
    );
}

/// 每个模块声明的命令名都必须出现在文档里。
///
/// 命令域归模块之后，"新增一条命令"改的是模块的 `COMMANDS` 常量，而
/// `document/` 里那份清单（`protocol.md` 的完整变体表、`modules.md` 的模块表）
/// 是手写的——漏登记不会有任何编译期信号。这里逐条问一遍"文档提过吗"。
///
/// 只查"出现过"，不查归属哪个模块：后者需要把散文结构与 `COMMANDS` 对齐，
/// 而那张表的说明列是人写的高信号内容，不值得为自动校验改写成机器形状。
#[test]
fn every_declared_command_is_mentioned_in_docs() {
    let commands: Vec<(&str, &[&str])> = vec![
        (
            "agent-module-session",
            &agent_module_session::commands::COMMANDS,
        ),
        (
            "agent-module-transcript",
            &agent_module_transcript::session_view::COMMANDS,
        ),
        ("agent-module-shell", &agent_module_shell::commands::COMMANDS),
        (
            "agent-module-federation",
            &agent_module_federation::commands::COMMANDS,
        ),
        (
            "agent-module-provider-llm",
            &agent_module_provider_llm::commands::COMMANDS,
        ),
    ];
    let mut docs = String::new();
    let mut stack = vec![repo_root().join("document")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                docs.push_str(&reading(&path));
            }
        }
    }

    let mut missing = Vec::new();
    for (crate_name, list) in commands {
        for name in list {
            if !docs.contains(name) {
                missing.push(format!("{crate_name}::{name}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "以下模块命令未出现在 document/ 任何一篇里（新增命令请登记到 protocol.md 的完整变体表）：\n{}",
        missing.join("\n")
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
