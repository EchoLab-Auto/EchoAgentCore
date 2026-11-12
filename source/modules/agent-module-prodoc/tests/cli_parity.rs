//! 与 `echo-prodoc check` 的**交叉验证**（`#[ignore]`，需要外部 fixture）。
//!
//! ## 为什么要有这个文件
//!
//! `check.rs` 在 Rust 里重新实现了 ProDoc CLI 的图结构检查。这是有意的重复
//! （agent 不能起 Node 进程），但重复就有分叉风险。单元测试只能证明"我认为的
//! 规则自洽"，证明不了"与 CLI 一致"。
//!
//! 所以这里留一个**手动跑**的对照入口：
//!
//! ```bash
//! # 1. 造一个带问题的文档目录（任意）
//! # 2. 跑 CLI，记下它的合计行
//! npx tsx packages/prodoc-cli/src/index.ts check /tmp/prodoc-fixture
//! # 3. 跑本测试，看两边数字是否一致
//! PRODOC_FIXTURE=/tmp/prodoc-fixture \
//!   cargo test -p agent-module-prodoc --test cli_parity -- --ignored --nocapture
//! ```
//!
//! ## 2026-11 的对照记录
//!
//! fixture：10 篇文档、6 条连线，含断链 / 自连 / 重复 id / 两个环 / 空正文 /
//! 无参数区 / 外链 / 相对路径引用 / 只写单轴坐标。
//!
//! | 项 | CLI | 本模块 |
//! |---|---|---|
//! | 文档数 | 10 | 10 |
//! | 连线数 | 6 | 6 |
//! | 错误 | 3（broken-link / duplicate-id / self-link） | 3（同） |
//! | 警告 | 7（cycle×2 / orphan×4 / empty-document） | 7（同） |
//! | 提示 | 2（partial-position / no-frame） | 2（同） |
//!
//! 已知差异（**不是 bug，是有意的子集**）：
//! - group 相关两项（多 group 声明 / 显式几何冲突）本模块不报；
//! - `broken-link` 的 `file` 字段：CLI 留空（它拿"目标名"反查不到），本模块
//!   填**声明方**——排查时更想知道"谁写错了"而不是"哪个名字找不到"。

use std::path::PathBuf;

use agent_module_prodoc::bundle;
use agent_module_prodoc::check;
use agent_module_prodoc::Config;

#[test]
#[ignore = "需要外部 fixture：PRODOC_FIXTURE=<文档根>"]
fn report_matches_prodoc_cli() {
    let root = std::env::var("PRODOC_FIXTURE").expect("设置 PRODOC_FIXTURE 指向文档根");
    let config = Config {
        roots: vec![root.clone()],
        ..Config::default()
    };
    let bundle = bundle::scan(&PathBuf::from(&root), &config).expect("扫描 fixture");
    let report = check::check(&bundle.files);

    println!("{}", report.summary());
    for issue in &report.issues {
        println!(
            "[{}] {} {}{}",
            issue.level.as_str(),
            issue.rule,
            issue.message,
            issue
                .file
                .as_deref()
                .map(|file| format!(" ({file})"))
                .unwrap_or_default()
        );
    }

    // 与 CLI 的合计行逐项对齐（数值改动时先跑一遍 CLI 再改这里）。
    assert_eq!(report.stats.documents, 10, "文档数（唯一 id）");
    assert_eq!(report.stats.relations, 6, "连线数");
    assert_eq!(report.stats.errors, 3, "错误数");
    assert_eq!(report.stats.warnings, 7, "警告数");

    let rules: Vec<&str> = report.issues.iter().map(|issue| issue.rule).collect();
    for expected in ["broken-link", "duplicate-id", "self-link", "cycle", "orphan", "empty-document"] {
        assert!(rules.contains(&expected), "缺 {expected}: {rules:?}");
    }
}
