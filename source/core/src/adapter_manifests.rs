//! 适配器插件的 manifest 声明（组合根侧）。
//!
//! **这是组合根的事实，不是框架的知识**：`echo-agent` 不认识任何平台名，
//! 适配器插件由本仓库（组合根）声明、由运行期注册表按 `kind == Adapter`
//! 推导出「哪些插件参与 persona 门控」。
//!
//! 迁出原因：这些 id 原先写在 `echo-agent::plugins` 的常量表里，等于让框架
//! 硬编码「有一个叫 qq 的平台」。加第二个平台时框架必须改代码——与
//! 「适配器是插件」的设计矛盾。
//!
//! 新增平台的做法（B 阶段会进一步泛化）：在 [`ADAPTER_PLUGIN_IDS`] 里加一行，
//! 并在 `main.rs` 里按 `[adapters.<platform>.plugin]` 声明 manifest。

/// QQ 适配器插件 id（`[adapters.qq]`；manifest 编译在 Core 里，实现在独立进程）。
pub const ADAPTER_QQ_PLUGIN_ID: &str = "echo-agent.adapter.qq";

/// 本组合根声明的全部适配器插件 id（安装后二进制里应有这些 manifest）。
///
/// 目前只被跨清单守护测试消费（因此 bin 构建下会报 dead_code）；B 阶段
/// 会用它驱动多平台 manifest 声明，届时进入生产路径。
#[cfg_attr(not(test), allow(dead_code))]
pub const ADAPTER_PLUGIN_IDS: [&str; 1] = [ADAPTER_QQ_PLUGIN_ID];

#[cfg(test)]
mod tests {
    use super::ADAPTER_PLUGIN_IDS;
    use std::path::Path;

    /// 跨清单守护：`scripts/update.sh::expected_ids` 必须等于
    /// 「框架内置插件 ∪ 本组合根声明的适配器插件」。
    ///
    /// 历史：两份清单曾静默漂移（`echo-agent.orchestration` 移除后残留在
    /// 脚本里整整一周，每次更新误报「缺少内置插件 manifest」）。本测试把
    /// 漂移变成 CI 失败。
    ///
    /// 放在单元测试里（而非 `tests/`）是因为适配器 id 在 bin crate 的模块中，
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

        let list = script
            .lines()
            .find(|line| line.contains("expected_ids="))
            .and_then(|line| line.split('"').nth(1))
            .expect("scripts/update.sh must define expected_ids=\"…\"");
        let mut script_ids: Vec<&str> = list.split_whitespace().collect();
        script_ids.sort_unstable();

        let mut declared: Vec<&str> = echo_agent::plugins::BUILTIN_PLUGIN_IDS.to_vec();
        declared.extend(ADAPTER_PLUGIN_IDS);
        declared.sort_unstable();

        assert_eq!(
            script_ids, declared,
            "scripts/update.sh expected_ids drifted from \
             BUILTIN_PLUGIN_IDS ∪ ADAPTER_PLUGIN_IDS — update both sides together \
             (see dev-guide「跨仓库清单同步」)"
        );
    }
}
