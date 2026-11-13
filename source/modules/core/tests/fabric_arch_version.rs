//! Guard: Core 自己声明的 `echo-fabric-arch` 版本必须唯一。
//!
//! ## 为什么
//!
//! `echo-fabric-arch` 提供 `echo_module::Module` trait。Core 与各个进程内模块
//! （Panel / subagent / workspace / prodoc）都靠它握手——拉到两个版本的
//! `echo-module` 时，同一个 trait 在两边是**不同的类型**，编译直接失败：
//!
//! ```text
//! expected `Module`, found `Module`
//! ```
//!
//! 这种错误信息几乎无法定位（名字一模一样）。历史靠人工同步版本号，踩过两次
//! （`5ccd12e` / `96d8b8e` 都是手工对齐 Panel 的 pin）。
//!
//! 本测试守**仓内**部分：Core 自己的多个 `Cargo.toml`（根 + 各 crate）必须声明
//! 同一个 tag。跨仓部分由 `scripts/check-fabric-arch-sync.sh` 守（它需要知道
//! 其它仓的路径，不适合放进 `cargo test`）。
//!
//! ## 为什么还要看 Cargo.lock
//!
//! `Cargo.toml` 的 tag 相同、`Cargo.lock` 的 rev 不同是可能的（有人跑过
//! `cargo update` 到 tag 的另一个提交，或者 tag 被移动过）。那种情况在 CI 全量
//! 构建时才会炸，所以这里一并钉住。

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

/// 收集仓内所有 `Cargo.toml`（根 + 成员 crate，跳过 target/vendor）。
fn cargo_tomls(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                // 依赖与构建产物里也有大量 Cargo.toml，与我们的声明无关。
                if matches!(name.as_ref(), "target" | "node_modules" | ".git" | "vendor") {
                    continue;
                }
                stack.push(path);
            } else if name == "Cargo.toml" {
                out.push(path);
            }
        }
    }
    out
}

/// 从一行里取出 fabric-arch 的 tag（`... echo-fabric-arch", tag = "v0.18.0"`）。
fn tag_from_line(line: &str) -> Option<String> {
    if !line.contains("echo-fabric-arch") || !line.contains("tag") {
        return None;
    }
    let after = line.split_once("tag")?.1;
    let rest = after.split_once('"')?.1;
    Some(rest.split_once('"')?.0.to_string())
}

#[test]
fn all_cargo_tomls_declare_one_fabric_arch_tag() {
    let root = repo_root();
    let mut seen: Vec<(String, PathBuf)> = Vec::new();
    for manifest in cargo_tomls(&root) {
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        for line in text.lines() {
            if let Some(tag) = tag_from_line(line) {
                seen.push((tag, manifest.clone()));
            }
        }
    }
    assert!(
        !seen.is_empty(),
        "仓内没有任何 echo-fabric-arch 依赖声明？扫描根：{}",
        root.display()
    );
    let mut tags: Vec<&str> = seen.iter().map(|(tag, _)| tag.as_str()).collect();
    tags.sort_unstable();
    tags.dedup();
    assert_eq!(
        tags.len(),
        1,
        "Core 内声明了多个 echo-fabric-arch 版本：{:?}\n{}",
        tags,
        seen.iter()
            .map(|(tag, path)| format!(
                "  {tag}  ←  {}",
                path.strip_prefix(&root).unwrap_or(path).display()
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// `Cargo.lock` 里只能有一个 `echo-module` 版本。
///
/// 两个版本意味着依赖图里存在两份 `Module` trait——正是上面那些编译错误的
/// 来源。`Cargo.lock` 是最终裁决，比 `Cargo.toml` 的声明更真实。
#[test]
fn lockfile_pins_a_single_echo_module() {
    let lock = repo_root().join("Cargo.lock");
    let text = std::fs::read_to_string(&lock).expect("读 Cargo.lock");
    let mut versions = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line == r#"name = "echo-module""# {
            for next in lines.by_ref() {
                if let Some(rest) = next.strip_prefix("version = ") {
                    versions.push(rest.trim_matches('"').to_string());
                    break;
                }
                if next.starts_with("[[") {
                    break;
                }
            }
        }
    }
    assert_eq!(
        versions.len(),
        1,
        "Cargo.lock 里 echo-module 有 {} 个版本：{versions:?}（会导致 Module trait 类型不一致）",
        versions.len()
    );
}
