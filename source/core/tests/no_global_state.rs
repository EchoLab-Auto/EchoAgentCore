//! P1 门禁（棘轮）：进程级静态可变状态的**只减不增**守卫。
//!
//! 背景：解耦计划 Phase 1 要求把全部
//! 进程级全局态收敛到唯一引导单元。2026-10-08 全量迁移完成。
//!
//! 口径（2026-10-19 技术债清理完成）：
//!
//! **两仓都是无条件零全局态**——白名单为空，没有任何豁免。
//!
//! 最后一处（`echo-bootstrap` 的 TypeId 键控单例表）已拆解为显式注入：
//! - 插件宿主 / 全局禁用策略 → `Agent` 字段（`plugin_host` / `global_policy`）
//! - 所属 manager → `Agent.manager`（弱引用，避免引用环）
//! - 人格重建工厂 → `AgentManager.factory`
//! - shell manager / 广播回调 → `ShellHandle`（组合根创建、clone 分发）
//! - 编译产物 / HTTP 连接池 → 注册期构造并捕获（不是宿主状态）
//! - 迁移 ack 表 → `FederationRuntime.session_import_acks`
//!
//! 副作用：同一进程现在可以起**两套完整宿主**，互不干扰。
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

/// 白名单：**空**（2026-10-19 技术债清理后无条件零全局态）。
///
/// 保留这个常量（而不是删掉分支）是为了让"将来若确有豁免"有现成的显式位置
/// ——比临时在扫描里加 `ends_with` 更难被忽略。
const WHITELIST: &str = "";

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
