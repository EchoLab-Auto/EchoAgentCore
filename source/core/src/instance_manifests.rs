//! 实例插件的 manifest 声明（组合根侧）。
//!
//! **这是组合根的事实，不是框架的知识**：`echo-agent` 不认识任何平台名，
//! 实例插件由运行期注册表按 `kind == PluginInstance` 推导出「哪些插件参与
//! persona 门控」。本模块只定义 **id 的形态**，不枚举平台。
//!
//! 历史：这些 id 原先写在 `echo-agent::plugins` 的常量表里，等于让框架硬编码
//! 「有一个叫 qq 的平台」，加第二个平台框架必须改代码。2026-10 去适配器化后
//! 连组合根也不点名平台——id 由 `[instances.<platform>]` 的段名派生。
//!
//! 新增平台的做法：配置里写 `[instances.<platform>]` 即可，不需要改任何代码。

/// 实例插件 id 前缀：完整 id = `<前缀><平台名>`（如 `echo-agent.instance.qq`）。
///
/// 保留为**字面量**是有意的：`scripts/update.sh` 安装后会在二进制里
/// `strings | grep` 校验这条前缀存在（守护测试见本模块 tests）。
pub const PLUGIN_ID_PREFIX: &str = "echo-agent.instance.";

/// 旧前缀（`echo-agent.adapter.<platform>`，2026-10 去适配器化前的形态）。
///
/// 用户配置的 `enabled_plugins`、技能文件 frontmatter 的 `package:` 里可能
/// 仍是旧 id——归一逻辑的唯一实现在
/// [`echo_agent::plugins::canonical_plugin_id`]（框架层，技能加载与 persona
/// 白名单共用），这里只保留常量供跨清单守护测试断言「脚本不再列旧前缀」。
#[cfg_attr(not(test), allow(dead_code))]
pub const LEGACY_PLUGIN_ID_PREFIX: &str = echo_agent::plugins::LEGACY_INSTANCE_PLUGIN_PREFIX;

/// 由平台名派生实例插件 id。
pub fn plugin_id_for(platform: &str) -> String {
    format!("{PLUGIN_ID_PREFIX}{platform}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn plugin_id_is_derived_from_platform() {
        assert_eq!(plugin_id_for("qq"), "echo-agent.instance.qq");
        assert_eq!(plugin_id_for("telegram"), "echo-agent.instance.telegram");
        // 派生结果里不含任何平台专属常量。
        assert!(plugin_id_for("qq").starts_with(PLUGIN_ID_PREFIX));
    }

    #[test]
    fn legacy_ids_are_canonicalised() {
        use echo_agent::plugins::canonical_plugin_id;
        assert_eq!(
            canonical_plugin_id("echo-agent.adapter.qq"),
            "echo-agent.instance.qq"
        );
        assert_eq!(
            canonical_plugin_id("echo-agent.instance.qq"),
            "echo-agent.instance.qq"
        );
        // 非旧前缀的其他 id（框架内置插件）一字不动。
        for id in [
            "echo-agent.tools.builtin",
            "echo-agent.skills.dir",
            "echo-agent.workspace",
            "echo-agent.subagent",
            "echo-agent.management.panel",
        ] {
            assert_eq!(canonical_plugin_id(id), id);
        }
    }

    /// 跨清单守护：`scripts/update.sh::expected_ids` 必须与
    /// `echo_agent::plugins::BUILTIN_PLUGIN_IDS` ∪ 本模块的 id **前缀**一致。
    ///
    /// 历史：两份清单曾静默漂移（`echo-agent.orchestration` 移除后残留在
    /// 脚本里整整一周，每次更新误报「缺少内置插件 manifest」）。本测试把
    /// 漂移变成 CI 失败。
    ///
    /// 实例插件 id 自 2026-10 起由平台名派生，因此脚本校验的是**前缀**
    /// 而不是逐个 id——`strings` 里能找到 `echo-agent.instance.` 就说明
    /// 这套派生逻辑进了二进制。
    ///
    /// 放在单元测试里（而非 `tests/`）是因为该模块在 bin crate 内，
    /// 集成测试看不到。
    #[test]
    fn update_script_expected_plugins_match_declared_ids() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("source/")
            .parent()
            .expect("repo root");
        let script_path = repo_root.join("scripts/update.sh");
        let script = std::fs::read_to_string(&script_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", script_path.display()));

        // 脚本里的固定清单（`expected_ids="..."` 那一行）。
        let line = script
            .lines()
            .find(|line| line.trim_start().starts_with("expected_ids="))
            .expect("update.sh 应有 expected_ids= 行");
        let listed: Vec<&str> = line
            .split('"')
            .nth(1)
            .unwrap_or_default()
            .split_whitespace()
            .collect();

        let mut declared: Vec<&str> = echo_agent::plugins::BUILTIN_PLUGIN_IDS.to_vec();
        // `echo-agent.orchestration` 已移除（2026-09），若脚本里还有它必须报错。
        assert!(
            !listed.contains(&"echo-agent.orchestration"),
            "update.sh 仍列着已删除的 echo-agent.orchestration"
        );
        declared.sort_unstable();
        let mut listed_sorted: Vec<&str> = listed
            .iter()
            .copied()
            .filter(|id| !id.starts_with(PLUGIN_ID_PREFIX))
            .collect();
        listed_sorted.sort_unstable();
        assert_eq!(
            listed_sorted, declared,
            "update.sh expected_ids 与 BUILTIN_PLUGIN_IDS 漂移"
        );
        assert!(
            listed.iter().any(|id| id.starts_with(PLUGIN_ID_PREFIX)),
            "update.sh 必须校验实例插件 id 前缀 {PLUGIN_ID_PREFIX}"
        );
        assert!(
            !listed
                .iter()
                .any(|id| id.starts_with(LEGACY_PLUGIN_ID_PREFIX)),
            "update.sh 不应再列旧前缀 {LEGACY_PLUGIN_ID_PREFIX}"
        );
    }
}
