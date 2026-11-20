//! `modules.toml` 用**非 builtin 行**顶替内置模块时，内置那份必须从目录里消失。
//!
//! 背景（2026-11）：把清单模块做成独立产物之后，`modules.toml` 里写
//!
//! ```toml
//! [[module]]
//! id = "echo-agent.checklist"
//! kind = "stdio"
//! command = ".../agent-module-checklist"
//! ```
//!
//! 结果 Core 起不来，两条硬错误：
//!
//! ```text
//! module id already registered: echo-agent.checklist
//! service key `echo-agent.checklist.tools` is already provided
//! ```
//!
//! 根因是装载器的"同 id 整行替换"只作用于**挂载计划**；模块目录是代码里静态
//! 构造的（`catalog_with` 无条件 `push_module` 了内置那份），所以同一个 id
//! 在目录里出现了两次。修复是让 Core 在装配期把"被外部条目顶替"的 id 从目录
//! 里摘掉。
//!
//! 这里守的是**判据本身**（哪些行算"顶替"、哪些不算），而不是整个启动流程：
//! 完整启动要真二进制 + 真子进程，属于集成测试的范畴，成本不该压在每个 PR 上。

use std::collections::HashSet;

/// 与 `main.rs` 里的实现同一条判据：非 builtin + enabled 才算"顶替"。
///
/// 复制一份而不是把它抽成公共函数：这条判据的输入是 `ModuleEntry`（加载器
/// 类型），抽公共函数要么给 Core 加一层导出、要么把判断挪进 `builtin_modules`
/// 而那里不依赖加载器。两行 filter 的成本低于一层间接——但**必须**与实现
/// 同步，所以这里逐条注释语义。
fn replaced_ids(entries: &[(String, String, bool)]) -> HashSet<String> {
    entries
        .iter()
        // `kind != "builtin"`：内置行是"配置它"，不是"换掉它"。
        .filter(|(_, kind, _)| kind != "builtin")
        // `enabled`：`enabled = false` 是"关掉"，内置那份仍留在目录里
        // （面板里随时能打开）；只有**启用**的外部条目才是"换个实现"。
        .filter(|(_, _, enabled)| *enabled)
        .map(|(id, _, _)| id.clone())
        .collect()
}

#[test]
fn stdio_row_replaces_the_builtin_entry() {
    let entries = vec![
        (
            "echo-agent.checklist".to_string(),
            "stdio".to_string(),
            true,
        ),
        (
            "echo-agent.panel.web".to_string(),
            "builtin".to_string(),
            true,
        ),
    ];
    let replaced = replaced_ids(&entries);
    assert!(replaced.contains("echo-agent.checklist"));
    assert!(
        !replaced.contains("echo-agent.panel.web"),
        "builtin 行是配置内置模块，不该把它从目录里摘掉"
    );
}

#[test]
fn disabled_row_does_not_replace_anything() {
    let entries = vec![(
        "echo-agent.checklist".to_string(),
        "stdio".to_string(),
        false,
    )];
    assert!(
        replaced_ids(&entries).is_empty(),
        "enabled = false 是「关掉」，不是「换个实现」：内置那份要留在目录里"
    );
}

#[test]
fn dylib_rows_also_replace() {
    let entries = vec![(
        "echo-agent.checklist".to_string(),
        "dylib".to_string(),
        true,
    )];
    assert!(replaced_ids(&entries).contains("echo-agent.checklist"));
}

/// 没有 `modules.toml`（`None`）时集合为空：休眠部署的行为完全不变。
#[test]
fn no_runtime_means_no_replacement() {
    let entries: Option<Vec<(String, String, bool)>> = None;
    let replaced = entries
        .map(|entries| replaced_ids(&entries))
        .unwrap_or_default();
    assert!(replaced.is_empty());
}
