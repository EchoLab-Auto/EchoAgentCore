//! 平台无关门禁：机制层 / 插件层不得出现任何**平台名**。
//!
//! ## 为什么需要
//!
//! 「新增平台 = 写一个实例插件，框架侧零改动」是插件化设计的核心承诺
//! （见 `document/overview/architecture.md`「扩展点地图」）。这条承诺靠两件
//! 事维持：协议与门控是平台中立的，以及**代码里没有平台分支**。前者有
//! `InstanceContribution` 与 `plugin_dependency_direction` 守着，后者此前
//! 只有「人看过一遍」——本文把它变成可断言的棘轮。
//!
//! ## 口径
//!
//! 扫描 `source/` 下除**业务层**（`backend/echo-agent`）与**组合根**（`core`）
//! 之外的全部 `.rs`，去掉注释与测试代码后匹配平台名。
//!
//! - **注释不算**：文档里举例（`"qq"`、QQ/OneBot）是说明，不是分支逻辑；
//! - **测试代码不算**：`#[cfg(test)]` 之后的代码、`tests/` 目录与
//!   `tests.rs` / `test_support.rs` 文件都是夹具，允许用具体平台做样例；
//! - **字符串字面量算**：`unwrap_or("qq")` 这类默认值才是真正会漏出去的耦合。
//!
//! ## 现状
//!
//! 2026-10 首跑即为 0 命中——Phase 3 的外迁把最后几处平台残留（`defs` 的
//! 文档举例、`host` 的 `instance.rs`）都变成了注释或测试。此后新增能力若
//! 需要平台名，应落在**实例插件**（独立仓库）里，或经贡献声明带出。

use std::path::{Path, PathBuf};

/// 平台名清单（小写；按词边界匹配）。
///
/// 只列「明确某个聊天平台」的标识——`adapter` / `instance` 这类中性词不列。
const PLATFORM_NAMES: [&str; 10] = [
    "qq", "telegram", "discord", "slack", "wechat", "weixin", "feishu", "dingtalk", "onebot",
    "napcat",
];

/// 业务层与组合根：允许出现平台名（业务本来就是为具体平台服务的）。
const EXEMPT_DIRS: [&str; 2] = ["backend", "core"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

/// 是否为测试代码文件（夹具允许使用具体平台）。
fn is_test_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let in_tests_dir = path.components().any(|c| c.as_os_str() == "tests");
    in_tests_dir || name == "tests.rs" || name == "test_support.rs" || name.starts_with("tests_")
}

/// 去掉注释与测试段，只留生产代码。
///
/// 手写解析（不引第三方）：只需正确跳过字符串字面量里的 `//`，以及
/// `#[cfg(test)]` 之后的整段。块注释按非贪婪跨行删除——本仓库没有嵌套块注释。
fn strip_to_production_code(text: &str) -> String {
    let text = strip_block_comments(text);
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        out.push_str(strip_line_comment(line));
        out.push('\n');
    }
    // 测试段（含 `#[cfg(test)] mod tests` 与其后的全部内容）
    match out.find("#[cfg(test)]") {
        Some(idx) => out[..idx].to_string(),
        None => out,
    }
}

/// 删块注释（`/* … */`，跨行）。
fn strip_block_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < bytes.len() {
                // 转义：原样带走下一个字符（可能是 `"`）
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] as char == '*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] as char == '*' && bytes[i + 1] as char == '/') {
                // 块注释里的换行要保留，行号才对得上（便于定位）
                if bytes[i] as char == '\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 删一行里位于字符串字面量之外的 `//` 注释。
fn strip_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if c == '/' && i + 1 < bytes.len() && bytes[i + 1] as char == '/' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

/// 该制品的生产代码里首个平台名命中（`(文件, 行号, 词)`）。
fn first_platform_hit(path: &Path) -> Option<(String, usize, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let code = strip_to_production_code(&text);
    for (index, line) in code.lines().enumerate() {
        let lowered = line.to_ascii_lowercase();
        for name in PLATFORM_NAMES {
            if contains_word(&lowered, name) {
                return Some((path.display().to_string(), index + 1, name.to_string()));
            }
        }
    }
    None
}

/// 按「词边界」判定：前后不是字母数字，避免 `qq` 命中 `qqq`。
fn contains_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 递归收集待扫描的 `.rs`（跳过业务层、组合根与测试夹具）。
fn production_sources(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                // target / node_modules / 测试目录一律跳过
                if matches!(name, "target" | "node_modules" | "tests") {
                    continue;
                }
                walk(&path, out);
            } else if name.ends_with(".rs") && !is_test_file(&path) {
                out.push(path);
            }
        }
    }
    let source = root.join("source");
    let mut out = Vec::new();
    let Ok(groups) = std::fs::read_dir(&source) else {
        return out;
    };
    for group in groups.flatten() {
        let name = group.file_name().to_string_lossy().to_string();
        if EXEMPT_DIRS.contains(&name.as_str()) {
            continue;
        }
        walk(&group.path(), &mut out);
    }
    out
}

#[test]
fn mechanism_and_plugin_crates_contain_no_platform_names() {
    let root = repo_root();
    let files = production_sources(&root);
    // 防真空：路径写错 / 全部被跳过时不该静默通过
    assert!(
        files.len() > 50,
        "只扫描到 {} 个源文件——扫描路径可能失效（预期整个 source/ 的机制与插件层）",
        files.len()
    );

    let mut hits = Vec::new();
    for file in &files {
        if let Some((path, line, name)) = first_platform_hit(file) {
            hits.push(format!("{path}:{line} 出现平台名 `{name}`"));
        }
    }
    assert!(
        hits.is_empty(),
        "机制层/插件层出现平台名——框架应当不认识任何平台；若确需平台语义，\n\
         请落到实例插件的贡献声明里，或把该文件登记进本测试的豁免清单：\n{}",
        hits.join("\n")
    );
}

/// 注释剥离器自检：注释里的平台名不算，字符串字面量里的算。
#[test]
fn comment_stripping_distinguishes_comments_from_code() {
    let sample = r#"
//! 文档里可以写 QQ / telegram
/// 举例 `"qq"` 也行
// 普通行注释：onebot
let x = 1; // 尾注释 discord
fn f() { /* 块注释 qq */ }
"#;
    let code = strip_to_production_code(sample);
    for name in ["qq", "telegram", "discord", "onebot"] {
        assert!(
            !contains_word(&code.to_ascii_lowercase(), name),
            "注释里的 {name} 不该被算作代码"
        );
    }

    let with_literal = r#"let platform = "qq";"#;
    let code = strip_to_production_code(with_literal);
    assert!(
        contains_word(&code.to_ascii_lowercase(), "qq"),
        "字符串字面量里的平台名必须被抓到"
    );

    // 字符串里的 `//` 不能当注释起点（URL 场景最容易踩）
    let url = r#"let base = "https://api.example.com/v1"; let p = "qq";"#;
    let code = strip_to_production_code(url);
    assert!(contains_word(&code.to_ascii_lowercase(), "qq"), "{code}");
    assert!(code.contains("https://api.example.com/v1"), "{code}");

    // 测试段整体不算
    let with_tests = "fn f() {}\n#[cfg(test)]\nmod tests { fn g() { let _ = \"qq\"; } }\n";
    let code = strip_to_production_code(with_tests);
    assert!(!contains_word(&code.to_ascii_lowercase(), "qq"), "{code}");
}

/// 词边界自检：`qqq` / `qq_number` 不命中 `qq`。
#[test]
fn word_boundary_does_not_match_substrings() {
    assert!(contains_word("qq", "qq"));
    assert!(contains_word("let qq = 1", "qq"));
    assert!(contains_word("platform.qq()", "qq"));
    assert!(!contains_word("qqq", "qq"));
    assert!(!contains_word("qq_number", "qq"));
    assert!(!contains_word("nqq", "qq"));
}
