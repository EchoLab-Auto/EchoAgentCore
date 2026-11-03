//! 插件**作用域**：谁能决定一个插件在某份人格里生效。
//!
//! ## 为什么不是一张插件 id 清单
//!
//! 此前门控集合是硬编码的 `GATED_PLUGIN_IDS: [&str; N]`——按**插件 id** 维护。
//! 而插件 id 是只增不减的东西：每加一个能力型插件，就要往 Core 常量补一行、
//! 往 Panel 常量补一行、再补 `update.sh` 的字面量。同一件事三份真相，任何一次
//! 跨仓改动都可能只改到一两份。实测已经错了三轮：
//!
//! | 时间 | 事故 | 后果 |
//! |---|---|---|
//! | 2026-10 | wire 名 `adapter`→`instance`，`gated_plugin_ids_from` 里的字符串没跟着改 | 实例插件永远进不了门控，`enabled_plugins` 管不住 QQ |
//! | 2026-11 | `shell` 从 `tools.builtin` 拆出，Core 补了 Panel 没补 | 面板上没有 shell 开关，用户无法启用 `bash-*` |
//! | 2026-11 | `session`/`transcript` 进了 Panel 清单、Core 没进 | 两个勾了不生效的假开关 |
//!
//! 三次都不是粗心，是**同一个结构问题**：清单的维度选错了。
//!
//! ## 现在的判据：`kind` → 作用域
//!
//! 门控真正想表达的是「这个插件向模型暴露工具/技能吗？暴露 → 该人格能关掉它」。
//! 这个判断的输入（manifest 的 `kind`）**早就存在**，现在被投影成一份 id 清单。
//! 按 `kind` 派生之后：
//!
//! - **插件会一直增加，`kind` 极少变**：新增一个 `tool` 插件不需要改这里；
//! - 只有引入**新 kind** 时才需要登记，而 `every_registered_kind_is_classified`
//!   会强制你思考它的作用域（fail-closed：未登记的 kind 视为进程级）；
//! - Panel 不再自己算（它拿 Core 下发的 `PluginInfo.gating`），跨仓漂移面归零。
//!
//! ## 与旧清单的等价性
//!
//! 改造前 `GATED_PLUGIN_IDS` 的 5 项 = `{tools.builtin, shell, workspace,
//! subagent}`（全部 `kind == "tool"`）+ `skills.dir`（`kind == "skill"`），
//! 加上运行期按 `kind == "instance"` 派生的实例插件。也就是说：
//!
//! ```text
//! kind ∈ {tool, skill, instance}  ≡  旧 GATED_PLUGIN_IDS ∪ 实例派生
//! ```
//!
//! 迁移第 1 步的 `derived_gating_matches_the_legacy_list` 逐项验证了这条等式；
//! 等式成立后旧常量才被删除。

/// 插件作用域：谁能决定它在这份人格里生效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginScope {
    /// 能力型：向模型暴露工具/技能，**每份人格各自决定开不开**
    /// （`enabled_plugins` 白名单生效，Panel 勾选区可见）。
    Persona,
    /// 进程级/基础设施：一个 Core 一份，人格白名单**不管它**
    /// （要关就卸载插件或在 `plugins.toml` 里停用）。
    Process,
}

impl PluginScope {
    /// 线格式取值（下发给 Panel 的 `PluginInfo.gating`）。
    ///
    /// 用字符串而不是枚举：`PluginInfo` 的其他字段（`kind` / `runtime`）也是
    /// 字符串，且老 Panel 必须能忽略这个字段而不反序列化失败。
    pub fn as_str(self) -> &'static str {
        match self {
            PluginScope::Persona => "persona",
            PluginScope::Process => "process",
        }
    }
}

/// `kind` → 作用域。**这是唯一的"清单"**，且它按类型而非个例维护。
///
/// 新增插件时通常不用碰它；新增 **kind** 时必须登记（测试守着）。
///
/// 归类理由（逐个写死，因为它就是设计决策本身）：
///
/// | kind | 作用域 | 为什么 |
/// |---|---|---|
/// | `tool` | Persona | 工具对模型可见——不同人格该有不同工具集 |
/// | `skill` | Persona | 同上（技能注入系统提示词） |
/// | `instance` | Persona | 平台实例决定"这个人格收不收 QQ 消息"，是人格级能力 |
/// | `provider` | Process | LLM 供应商是进程级配置；人格级选择走 `persona_api` 字段，不走白名单 |
/// | `loop` | Process | 循环驱动是进程级单例，不是"关掉某个插件" |
/// | `management` | Process | Panel 连接面——关掉即自锁，不该出现在人格勾选区 |
/// | `federation` | Process | Core↔Core 链路是部署形状，不是人格能力 |
/// | `orchestration` | Process | 自更新等运维能力，进程级 |
/// | `session` | Process | 会话 trunk 是数据层；关掉它等于"没有人格"，不是可选项 |
/// | `interaction` | Process | 显示投影/人在环交互面，进程级 |
pub const KIND_SCOPE: [(&str, PluginScope); 12] = [
    ("tool", PluginScope::Persona),
    ("skill", PluginScope::Persona),
    // 实例类 kind 引用宿主常量而非字面量：`2d46b7d` 把 wire 名从 `adapter`
    // 改成 `instance` 时，硬编码比较的地方漏改，实例插件永远进不了门控集合。
    (super::INSTANCE_PLUGIN_KIND, PluginScope::Persona),
    ("provider", PluginScope::Process),
    ("loop", PluginScope::Process),
    ("management", PluginScope::Process),
    ("federation", PluginScope::Process),
    ("orchestration", PluginScope::Process),
    ("session", PluginScope::Process),
    ("interaction", PluginScope::Process),
    // 安全能力是**不变量**，不是某个人格的选项：脱敏被关掉等于密钥明文
    // 外泄，所以它归进程级（人格白名单碰不到它）。
    ("security", PluginScope::Process),
    // 运行地基（2026-11：取消「中间层 / 插件」二分后新增）。词汇、线契约、
    // 工具注册表、事件日志、配置存储、循环驱动——它们是其它插件的地基，
    // 既不该被哪个 persona 关掉，也不该被 `plugins.toml` 关掉。
    ("kernel", PluginScope::Process),
];

/// 某 kind 的作用域。
///
/// **未登记的 kind 一律 `Process`（fail-closed）**：能力扩散必须是显式决定。
/// 反过来（默认 `Persona`）会让新 kind 的插件悄悄从所有白名单里掉出去——
/// 表现为"用户明明勾了却用不了"，比"多显示一个无效开关"难查得多。
pub fn scope_of_kind(kind: &str) -> PluginScope {
    KIND_SCOPE
        .iter()
        .find(|(known, _)| *known == kind)
        .map(|(_, scope)| *scope)
        .unwrap_or(PluginScope::Process)
}

/// kind 是否已在 [`KIND_SCOPE`] 登记（组合根用它做装配期校验）。
pub fn kind_is_classified(kind: &str) -> bool {
    KIND_SCOPE.iter().any(|(known, _)| *known == kind)
}

/// 受 persona 名单门控的插件 id 集合（从注册表按 `kind` 派生）。
///
/// 取代原 `GATED_PLUGIN_IDS` + `gated_plugin_ids_from` 里那段"固定清单 ∪ 实例"
/// 的双轨逻辑——两条轨现在同一条规则。
pub fn persona_scoped_ids(registered: &[(String, String)]) -> Vec<String> {
    registered
        .iter()
        .filter(|(_, kind)| scope_of_kind(kind) == PluginScope::Persona)
        .map(|(id, _)| id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(id: &str, kind: &str) -> (String, String) {
        (id.to_string(), kind.to_string())
    }

    /// 五个历史门控项 + 实例插件，在按 kind 派生后**逐项仍在集合里**。
    ///
    /// 这条断言是删旧常量 `GATED_PLUGIN_IDS` 的安全绳——没有它，删常量就是一次
    /// 盲删。清单照抄历史值（故意不引用常量：常量一删就没有对照物了）。
    ///
    /// 用合成 kind 而非真 catalog：本 crate 看不到 core 的 catalog（依赖方向），
    /// 真实 catalog 的对齐由 `source/core/src/builtin_plugins.rs` 的
    /// `every_catalog_kind_is_classified` + `catalog_gating_matches_legacy_intent`
    /// 两条测试守着——那边能拿到真 manifest，这边只验规则。
    #[test]
    fn derived_gating_covers_the_five_legacy_entries_plus_instances() {
        // 十三项与它们在 catalog 里的真实 kind（由 core 侧测试交叉验证）。
        let registered: Vec<(String, String)> = vec![
            desc("echo-agent.tools.builtin", "tool"),
            desc("echo-agent.shell", "tool"),
            desc("echo-agent.workspace", "tool"),
            desc("echo-agent.subagent", "tool"),
            desc("echo-agent.skills.dir", "skill"),
            desc("echo-agent.instance.qq", "instance"),
            desc("echo-agent.provider.llm", "provider"),
            desc("echo-agent.loop.single", "loop"),
            desc("echo-agent.management.panel", "management"),
            desc("echo-agent.federation", "federation"),
            desc("echo-agent.selfupdate", "orchestration"),
            desc("echo-agent.session", "session"),
            desc("echo-agent.transcript", "interaction"),
        ];

        let derived = persona_scoped_ids(&registered);

        // 迁移前 `GATED_PLUGIN_IDS` 的 5 项（照抄历史值，故意不引用常量：
        // 常量一删这条测试就没有对照物了）。
        let legacy = [
            "echo-agent.tools.builtin",
            "echo-agent.skills.dir",
            "echo-agent.workspace",
            "echo-agent.subagent",
            "echo-agent.shell",
        ];
        for id in legacy {
            assert!(
                derived.iter().any(|d| d == id),
                "旧门控项 {id} 未被派生集合覆盖，实际: {derived:?}"
            );
        }
        assert!(
            derived.iter().any(|d| d == "echo-agent.instance.qq"),
            "实例插件必须进派生集合，实际: {derived:?}"
        );

        // 反向：派生集合里**不该**出现进程级插件（它们之前就不在清单里）。
        for id in [
            "echo-agent.provider.llm",
            "echo-agent.loop.single",
            "echo-agent.management.panel",
            "echo-agent.federation",
            "echo-agent.selfupdate",
            "echo-agent.session",
            "echo-agent.transcript",
        ] {
            assert!(
                !derived.iter().any(|d| d == id),
                "进程级插件 {id} 不该进 persona 门控集合，实际: {derived:?}"
            );
        }
    }

    /// 未登记的 kind 必须 fail-closed 到进程级（能力扩散是显式决定）。
    #[test]
    fn unknown_kind_is_process_scoped() {
        assert_eq!(scope_of_kind("brand-new-kind"), PluginScope::Process);
        assert!(!kind_is_classified("brand-new-kind"));
        let derived = persona_scoped_ids(&[desc("echo-agent.x", "brand-new-kind")]);
        assert!(
            derived.is_empty(),
            "未知 kind 不该进 persona 集合: {derived:?}"
        );
    }

    /// 新增一个 `tool` 插件**不需要改任何门控清单**——这是本设计的核心收益。
    #[test]
    fn a_new_tool_plugin_needs_no_gating_list_change() {
        let derived = persona_scoped_ids(&[desc("echo-agent.vision", "tool")]);
        assert_eq!(derived, vec!["echo-agent.vision".to_string()]);
    }

    /// 线格式取值稳定（Panel 与本仓跨仓库，字符串即契约）。
    #[test]
    fn scope_wire_names_are_stable() {
        assert_eq!(PluginScope::Persona.as_str(), "persona");
        assert_eq!(PluginScope::Process.as_str(), "process");
    }
}
