//! P1 门禁（棘轮）：进程级静态可变状态的**只减不增**守卫。
//!
//! 背景：解耦计划 Phase 1（见 `document/decoupling-plan.md`）要求把全部
//! 进程级全局态迁移到 `Ctx` 服务键。迁移期间本测试锁定存量上限：
//! **每迁移一处就把 `EXPECTED_MAX` 调小一格——只降不升**。
//!
//! 口径（2026-10-07 精化，全 source 扫描）：
//! - 扫描仓库 `source/` 全树，跳过 `tests/` 目录与 `tests.rs` 文件（只数生产代码）；
//! - 每文件剥离 `#[cfg(test)]` 之后的代码；
//! - 计数单位 = 「同一行同时含 `static` 与 `OnceLock`/`LazyLock`」的声明行
//!   （实例字段 / 局部非静态用法不计）。
//!
//! 允许的例外：`LazyLock` 用于**纯不可变缓存**（如正则、HTTP client）需在评审中
//! 说明理由；其余新增都会撞上棘轮。

use std::path::{Path, PathBuf};

/// 存量上限（2026-10-07 基线 17；每完成一处迁移调小一格，永不调大）。
const EXPECTED_MAX: usize = 17;

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
    // 剥离内联测试块（其内的声明不算生产代码）
    let text = match text.find("#[cfg(test)]") {
        Some(i) => &text[..i],
        None => text.as_str(),
    };
    text.lines()
        .filter(|line| {
            line.contains("static")
                && (line.contains("OnceLock") || line.contains("LazyLock"))
        })
        .count()
}

fn scan(dir: &Path, out: &mut Vec<(PathBuf, usize)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // 测试目录不进扫描（只数生产代码）
            if path.file_name().and_then(|n| n.to_str()) == Some("tests") {
                continue;
            }
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
    scan(&root.join("source"), &mut files);
    let total: usize = files.iter().map(|(_, n)| n).sum();
    let listing = files
        .iter()
        .map(|(p, n)| format!("  {}: {n}", p.strip_prefix(&root).unwrap_or(p).display()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        total <= EXPECTED_MAX,
        "进程级静态单元增加了：{total} 处 > 上限 {EXPECTED_MAX}。\n\
         P1 只允许减少（迁移到 Ctx 服务键），禁止新增进程级全局态。\n\
         当前分布：\n{listing}"
    );
    eprintln!("进程级静态单元存量：{total}（上限 {EXPECTED_MAX}）\n{listing}");
}
