//! P1 门禁（棘轮）：进程级静态可变状态的**只减不增**守卫。
//!
//! 背景：解耦计划 Phase 1（见 `document/design/decoupling-plan.md`）要求把全部
//! 进程级全局态收敛到唯一引导单元。2026-10-08 全量迁移完成。
//!
//! 口径（2026-10-19 M4）：
//!
//! - **框架仓**（`plugin-framework`）零全局态，无条件成立——白名单为空，由那边
//!   的 `crates/framework-gates/tests/no_global_state.rs` 守着；
//! - **本仓**唯一允许的静态存储是 `context/echo-bootstrap`（进程级引导单元）。
//!   它曾随 `echo-context` 一起被迁进框架仓，是错的：使用者全在应用层（8 处），
//!   框架自己一处不用，而框架导出进程级单例表会直接破坏"同一进程能起两套宿主"。
//!   现已搬回本仓，本门禁重新精确豁免这一个文件。
//!
//! 口径（2026-10-07 精化，全 source 扫描）：
//! - 扫描仓库 `source/` 全树，跳过 `tests/` 目录与 `tests.rs` 文件（只数生产代码）；
//! - 每文件剥离 `#[cfg(test)]` 之后的代码；
//! - 计数单位 = 「同一行同时含 `static` 与 `OnceLock`/`LazyLock`」的声明行
//!   （实例字段 / 局部非静态用法不计）。
//!
//! **唯一白名单**：`source/context/echo-context/src/kernel.rs`（kernel
//! cell 本体）——其 `KERNEL` 是全 source 唯一剩余的进程级静态存储，
//! 即旧 `OnceLock` 静态的收敛点；扫描时整体跳过。

use std::path::{Path, PathBuf};

/// 存量上限（2026-10-08：全量迁移完成 → 0；只降不升，永不调大）。
const EXPECTED_MAX: usize = 0;

/// 唯一白名单：宿主侧进程级引导单元（`echo-bootstrap`）。
///
/// **它是唯一允许的**——新增第二处即红。搬到本仓而不是留在框架仓的理由见
/// 该 crate 的文档；若将来把句柄改由 `Ctx` 显式注入，这条豁免应随之清零。
const WHITELIST: &str = "context/echo-bootstrap/src/lib.rs";

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
            line.contains("static") && (line.contains("OnceLock") || line.contains("LazyLock"))
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
            // 白名单为空时不跳过任何文件。
            if !WHITELIST.is_empty() && path.ends_with(WHITELIST) {
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
    let source_dir = root.join("source");
    // 白名单非空时必须真实存在——防止豁免在被审计文件移动后静默失效。
    if !WHITELIST.is_empty() {
        assert!(
            source_dir.join(WHITELIST).is_file(),
            "白名单文件不存在：source/{WHITELIST}（被移动或删除？）"
        );
    }
    let mut files = Vec::new();
    scan(&source_dir, &mut files);
    let total: usize = files.iter().map(|(_, n)| n).sum();
    let listing = files
        .iter()
        .map(|(p, n)| format!("  {}: {n}", p.strip_prefix(&root).unwrap_or(p).display()))
        .collect::<Vec<_>>()
        .join("\n");
    // 相等断言（棘轮语义）：既不许增加（新增全局态），也不许悄悄"少算"
    // 而不调低 EXPECTED_MAX——迁移时必须同步把 EXPECTED_MAX 调到当前值。
    assert_eq!(
        total, EXPECTED_MAX,
        "进程级静态单元数 {total} != 期望 {EXPECTED_MAX}。\n\
         P1：除宿主引导单元（source/{WHITELIST}）外，禁止任何进程级全局态；\n\
         若你刚迁移了一处，请把 EXPECTED_MAX 调低到 {total}（只降不升）。\n\
         当前分布：\n{listing}"
    );
    eprintln!("进程级静态单元存量：{total}（期望 {EXPECTED_MAX}）\n{listing}");
}
