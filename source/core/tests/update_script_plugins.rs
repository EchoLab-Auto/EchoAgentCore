//! Cross-list guard: the updater's plugin checklist must match the code.
//!
//! `scripts/update.sh` verifies a freshly built binary against a hardcoded
//! `expected_ids` list. That list is a copy of
//! [`echo_agent::plugins::BUILTIN_PLUGIN_IDS`]; when a plugin is removed the
//! two drifted silently and every update printed a bogus "缺少内置插件
//! manifest" warning (`echo-agent.orchestration` lingered for a week after the
//! orchestration plugin was removed on 2026-09-16). This test turns that drift
//! into a CI failure.
//!
//! `dev-guide` 的「跨仓库清单同步」把本测试列为守护之一；改插件清单时
//! `BUILTIN_PLUGIN_IDS` 与 `scripts/update.sh::expected_ids` 必须同改。

use std::path::Path;

#[test]
fn update_script_expected_plugins_match_builtin_ids() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("source/")
        .parent()
        .expect("repo root");
    let script_path = repo_root.join("scripts/update.sh");
    let script = std::fs::read_to_string(&script_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", script_path.display()));

    let list = script
        .lines()
        .find(|line| line.contains("expected_ids="))
        .and_then(|line| line.split('"').nth(1))
        .expect("scripts/update.sh must define expected_ids=\"…\"");
    let mut script_ids: Vec<&str> = list.split_whitespace().collect();
    script_ids.sort_unstable();

    let mut builtin_ids: Vec<&str> = echo_agent::plugins::BUILTIN_PLUGIN_IDS.to_vec();
    builtin_ids.sort_unstable();

    assert_eq!(
        script_ids, builtin_ids,
        "scripts/update.sh expected_ids drifted from BUILTIN_PLUGIN_IDS — \
         update both lists together (see dev-guide「跨仓库清单同步」)"
    );
}
