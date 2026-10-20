//! 框架层**领域无关性**门禁（M2，2026-10-19）。
//!
//! 背景：`document/design/plugin-framework-extraction.md` 冻结了框架/应用边界，
//! 判据之一是「框架 crate 零领域依赖、不认识应用词汇」。这条判据此前只写在
//! 文档里——靠人读，不靠机器。本测试把它变成可执行断言：
//!
//! 1. 框架 crate 的**生产代码**（`src/` 递归，`#[cfg(test)]` 之后不算）里
//!    不许出现应用领域词汇（工具 / 技能 / 会话 / 时间线 / 人格 …）；
//! 2. 框架 crate 的 `Cargo.toml` 不许依赖定义层（`echo-defs`）与业务内核
//!    （`echo-agent`）——领域适配是 `echo-plugin-adapter` 的职责。
//!
//! 已迁出本仓的两个框架 crate（`echo-plugin-api` / `echo-plugin-sdk`，见
//! `plugin-framework` 仓库）由那边的同名门禁守着；本文件在 M3 六件套齐迁后
//! 整体搬过去。
//!
//! 为什么用门禁而不是评审：`MountContext.tools` / `PluginKind::{Tool,Skill}` /
//! `PluginToolSink` 这类东西**当初都是善意加的**（"给外部插件预留的接缝"），
//! 但它们让框架认识了它不该认识的词。人读文档会漏，编译器和测试不会。

use std::path::{Path, PathBuf};

/// 框架 crate（相对仓库根 `source/`）。
const FRAMEWORK_CRATES: &[&str] = &[
    "context/echo-context",
    "plugin/echo-plugin",
    "plugin/echo-plugin-loader",
    "plugin/echo-plugin-host",
];

/// 框架层禁用的应用领域词汇。
///
/// 选词标准：**只有宿主应用才有意义的词**。刻意不含 `plugin` / `manifest` /
/// `service` / `event` / `transport` 这类框架自身词汇——框架当然可以有这些。
const FORBIDDEN_WORDS: &[&str] = &[
    "session",
    "timeline",
    "persona",
    "team",
    "prompt",
    "llm",
    "tool",
    "skill",
    "workspace",
    "chat",
    "agent",
];

/// 框架 crate 不许依赖的 echo-* crate（领域层 + 业务内核）。
const FORBIDDEN_DEPS: &[&str] = &["echo-defs", "echo-agent", "echo-protocol", "echo-session"];

fn repo_root() -> PathBuf {
    // 本文件位于 <repo>/source/core/tests/ → source/core → source
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

/// 递归收集 `dir` 下的 `.rs` 文件（跳过 `bin/`）。
///
/// `src/bin/` 里只有 conformance 对端夹具（`echo-plugin-test-peer`），它的职责
/// 就是构造协议的各种贡献——包括协议目前仍带的领域分类（M5 待议）。它不参与
/// 对外 API 表面，因此不算"框架认识了领域词汇"。
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "bin") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// 生产代码 = `#[cfg(test)]` 之前的部分（测试里可以出现任何词）。
fn production_part(text: &str) -> &str {
    match text.find("#[cfg(test)]") {
        Some(idx) => &text[..idx],
        None => text,
    }
}

/// 整词匹配：前后都不能是标识符字符（避免 `tools` 命中 `tool`、
/// `EchoAgentCore` 命中 `agent`）。
fn contains_word(line: &str, word: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    // 大小写不敏感：Rust 里是 `SessionStore` / `LlmProvider` 这种 CamelCase。
    // `to_ascii_lowercase` 只改 ASCII 字节，长度不变，索引可以直接复用。
    let line = line.to_ascii_lowercase();
    let mut start = 0;
    while let Some(pos) = line[start..].find(word) {
        let begin = start + pos;
        let end = begin + word.len();
        let before_ok = line[..begin]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident(c));
        let after_ok = line[end..].chars().next().is_none_or(|c| !is_ident(c));
        if before_ok && after_ok {
            return true;
        }
        start = begin + 1;
        if start >= line.len() {
            break;
        }
    }
    false
}

/// 从 `Cargo.toml` 取 `[dependencies]` / `[dev-dependencies]` 里的依赖名。
fn declared_deps(manifest: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return Vec::new();
    };
    let mut in_deps = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_deps = line == "[dependencies]" || line == "[dev-dependencies]";
            continue;
        }
        if !in_deps || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = line.split_once('=') {
            out.push(name.trim().to_string());
        }
    }
    out
}

#[test]
fn framework_production_code_has_no_domain_vocabulary() {
    let root = repo_root();
    let mut violations = Vec::new();

    for crate_dir in FRAMEWORK_CRATES {
        let src = root.join("source").join(crate_dir).join("src");
        assert!(
            src.is_dir(),
            "框架 crate 不存在：{}——M3 迁出后请把本门禁一并搬到框架仓",
            src.display()
        );
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for (idx, line) in production_part(&text).lines().enumerate() {
                for word in FORBIDDEN_WORDS {
                    if contains_word(line, word) {
                        violations.push(format!(
                            "{}:{}: 「{word}」— {}",
                            file.strip_prefix(&root).unwrap_or(&file).display(),
                            idx + 1,
                            line.trim()
                        ));
                    }
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "框架层出现了应用领域词汇（框架不该认识它们；要注册领域贡献请走 \
         `Ctx::provide` 服务键，或放进宿主侧的适配层）：\n{}",
        violations.join("\n")
    );
}

#[test]
fn framework_crates_do_not_depend_on_domain_or_kernel_crates() {
    let root = repo_root();
    let mut violations = Vec::new();

    for crate_dir in FRAMEWORK_CRATES {
        let manifest = root.join("source").join(crate_dir).join("Cargo.toml");
        for dep in declared_deps(&manifest) {
            if FORBIDDEN_DEPS.contains(&dep.as_str()) {
                violations.push(format!("{crate_dir} → {dep}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "框架 crate 依赖了领域/内核 crate（领域适配应落在 echo-plugin-adapter）：\n{}",
        violations.join("\n")
    );
}

/// 门禁自检：确认整词匹配不会误报，也不会漏报。
///
/// 没有这条，门禁可能在"永远为空"的状态下静默失效（例如有人把
/// `production_part` 改坏成返回空串）。
#[test]
fn word_boundary_matching_is_sound() {
    // 不该命中：复数 / 复合词
    assert!(!contains_word("let tools = 1;", "tool"));
    assert!(!contains_word("// EchoAgentCore brand", "agent"));
    assert!(!contains_word("skills_dir", "skill"));

    // 该命中：整词
    assert!(contains_word("let tool = 1;", "tool"));
    assert!(contains_word("/// a live agent", "agent"));
    assert!(contains_word("pub struct Session", "session"));
}

/// 门禁自检：`#[cfg(test)]` 之后的内容必须被忽略。
#[test]
fn test_modules_are_excluded() {
    let src = "pub fn f() {}\n#[cfg(test)]\nmod tests { fn g() { let tool = 1; } }";
    assert!(!contains_word(production_part(src), "tool"));
}
