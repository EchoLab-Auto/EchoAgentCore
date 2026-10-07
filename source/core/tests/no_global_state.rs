//! P1 门禁（棘轮）：进程级全局可变状态的**只减不增**守卫。
//!
//! 背景：解耦计划 Phase 1（见 `document/decoupling-plan.md`）要求把全部
//! 进程级全局态（`OnceLock` 等）迁移到 `Ctx` 服务键。迁移期间本测试锁定
//! 存量上限：**每迁移一处就把 `EXPECTED_MAX` 调小一格——只降不升**。
//!
//! 扫描范围：`source/backend/echo-agent/src` 与 `source/core/src` 下全部
//! `.rs`（剥离 `#[cfg(test)]` 之后的代码、跳过 `tests.rs` 专用测试文件）。

use std::path::{Path, PathBuf};

/// 存量上限（2026-10-07 基线 34；每完成一处迁移调小一格，永不调大）。
const EXPECTED_MAX: usize = 34;

fn repo_root() -> PathBuf {
    // 本文件位于 source/core/tests/ → source/core → 仓库根
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn count_in_file(path: &Path) -> usize {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    // 剥离内联测试块（其内的 Occurrence 不算全局态）
    let text = match text.find("#[cfg(test)]") {
        Some(i) => &text[..i],
        None => text.as_str(),
    };
    text.matches("OnceLock").count()
}

fn scan(dir: &Path, out: &mut Vec<(PathBuf, usize)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "tests.rs" {
                continue;
            }
            let n = count_in_file(&path);
            if n > 0 {
                out.push((path, n));
            }
        }
    }
}

#[test]
fn global_state_ratchet_only_decreases() {
    let root = repo_root();
    let mut files = Vec::new();
    for base in ["source/backend/echo-agent/src", "source/core/src"] {
        scan(&root.join(base), &mut files);
    }
    let total: usize = files.iter().map(|(_, n)| n).sum();
    let listing = files
        .iter()
        .map(|(p, n)| format!("  {}: {n}", p.strip_prefix(&root).unwrap_or(p).display()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        total <= EXPECTED_MAX,
        "全局态增加了：{total} 处 > 上限 {EXPECTED_MAX}。\n\
         P1 只允许减少（迁移到 Ctx 服务键），禁止新增进程级全局态。\n\
         当前分布：\n{listing}"
    );
    // 迁移推进时：把 EXPECTED_MAX 调小到当前 total，防止回涨。
    eprintln!("全局态存量：{total}（上限 {EXPECTED_MAX}）\n{listing}");
}
