//! 插件 id 词汇：实例插件的命名空间与前缀归一化。
//!
//! 插件 id 是**配置契约**（`enabled_plugins` 白名单、SKILL.md 的 `package:`
//! 标签、`plugins.toml` 的行 id、Panel 插件清单），因此它的拼写属于定义层，
//! 不属于任何一个实现方——框架、组合根、技能加载器共用同一套归一口径，
//! 才不会出现「升级后技能与工具的包标签对不上、面板分成两个包」。
//!
//! 本模块**不认识任何平台名**：只做前缀替换，平台名由调用方带入。

/// 实例插件 id 的旧前缀（`echo-agent.adapter.<platform>`）。
///
/// 2026-10 去适配器化把实例插件 id 改成了
/// [`INSTANCE_PLUGIN_PREFIX`]，但用户配置的 `enabled_plugins`、SKILL.md 的
/// `package:` 里仍是旧 id，故读侧统一归一。
pub const LEGACY_INSTANCE_PLUGIN_PREFIX: &str = "echo-agent.adapter.";

/// 实例插件 id 的现役前缀（`echo-agent.instance.<platform>`）。
pub const INSTANCE_PLUGIN_PREFIX: &str = "echo-agent.instance.";

/// 把旧实例插件 id 归一为现役形态；其余 id 原样返回。
pub fn canonical_plugin_id(id: &str) -> String {
    match id.strip_prefix(LEGACY_INSTANCE_PLUGIN_PREFIX) {
        Some(platform) => format!("{INSTANCE_PLUGIN_PREFIX}{platform}"),
        None => id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_instance_prefix_is_canonicalized() {
        assert_eq!(
            canonical_plugin_id("echo-agent.adapter.qq"),
            "echo-agent.instance.qq"
        );
        // 平台名原样保留（本模块不认识平台）
        assert_eq!(
            canonical_plugin_id("echo-agent.adapter.whatever"),
            "echo-agent.instance.whatever"
        );
    }

    #[test]
    fn current_and_framework_ids_pass_through() {
        assert_eq!(
            canonical_plugin_id("echo-agent.instance.qq"),
            "echo-agent.instance.qq"
        );
        assert_eq!(
            canonical_plugin_id("echo-agent.tools.builtin"),
            "echo-agent.tools.builtin"
        );
        assert_eq!(canonical_plugin_id(""), "");
    }

    #[test]
    fn only_the_instance_namespace_is_rewritten() {
        // 前缀出现在中间不算（避免误伤 `echo-agent.meta.adapter.x`）
        assert_eq!(
            canonical_plugin_id("my.echo-agent.adapter.qq"),
            "my.echo-agent.adapter.qq"
        );
    }
}
