//! # 模块技能贡献适配
//!
//! 把模块的 [`SkillContribution`]（`Custom{kind: "echo-agent.skill"}`）桥接
//! 为宿主侧 [`echo_defs::skill::Skill`]，交给技能表按 `owner` 注册。
//!
//! ## 与工具适配的分工
//!
//! 工具贡献是**调用先于存在**：适配成 `Tool` trait 对象后放进 `ToolRegistry`，
//! 模型一调就走远程 `Invoke`。技能贡献是**纯数据**：内容随 `Register` 帧
//! 一次性到手，之后宿主不再回问模块——所以这里只做解码 + 解析，不产生任何
//! 持有模块句柄的活对象。
//!
//! ## 为什么 owner 是模块 id
//!
//! 技能表要能回答「这批技能是谁带来的」，才能在模块卸载 / 热替换 / 被禁用
//! 时把对应的一批撤掉（`SkillRegistry::set_contributed` 的替换语义）。
//! 模块 id 是唯一稳定的归属标识；`SkillContribution::package` 另作**门控**
//! 标签用（跨类型的包聚合），两者不同层，别混。

use echo_defs::skill::Skill;
use echo_module_host::ModuleHandle;

use crate::adapter::contract::{skill_from_custom, SkillContribution};

/// 模块句柄的技能视图（扩展 trait，理由同 [`crate::adapter::ModuleHandleTools`]：
/// `ModuleHandle` 定义在领域中立的框架 crate 里，加不了 inherent 方法）。
pub trait ModuleHandleSkills {
    /// 本模块贡献的技能，`owner` = 模块 id。
    ///
    /// 返回 `(owner, skills)` 而**不是**平铺的 `Vec<Skill>`：调用方
    /// （`SkillContributionSource` 的实现）要把多模块的贡献合起来交给技能表，
    /// 而技能表按 owner 做整体替换——平铺会丢掉归属信息。
    fn contributed_skills(&self) -> (String, Vec<Skill>);
}

impl ModuleHandleSkills for ModuleHandle {
    fn contributed_skills(&self) -> (String, Vec<Skill>) {
        let skills: Vec<Skill> = self
            .contributions()
            .into_iter()
            .filter_map(|contribution| skill_from_custom(&contribution).map(into_skill))
            .collect();
        (owner_of(&self.module_id().to_string(), &skills), skills)
    }
}

/// 归属键：这批技能**属于谁**（撤销时整批撤的单位）。
///
/// ## 为什么不是直接拿模块 id
///
/// 平台模块的每个实例是**独立进程**，各自 `Register` 同一份技能：QQ 模块
/// 跑 4 个实例（qq / qq-2 / base / EchoCode），module id 是
/// `instance-qq-qq`、`instance-qq-base`… 各不相同。用 id 当归属会得到 4 个
/// 伪 owner：
///
/// - 面板/日志里同一份技能像来自 4 个来源（误导）；
/// - 某个实例重启时它那一份"消失再出现"，注册表来回抖；
/// - 撤销要靠引用计数兜底（见 `SkillRegistry::drop_if_unclaimed`）——
///   那是必要的最后防线，但归属本身就不该分裂。
///
/// 判据是**这批技能声明的 package 是否一致**：一致（`echo-agent.instance.qq`）
/// 就说明它们同属一个包，包 id 是比进程更稳的归属标识；不一致或没声明时
/// 退回 module id（内置模块的 subagent / prodoc 走这条，它们本来就只有一个
/// 来源）。
fn owner_of(module_id: &str, skills: &[Skill]) -> String {
    let mut packages = skills.iter().filter_map(|s| s.metadata.package.as_deref());
    let Some(first) = packages.next() else {
        return module_id.to_string();
    };
    if packages.all(|package| package == first) {
        first.to_string()
    } else {
        module_id.to_string()
    }
}

/// `SkillContribution` → 宿主技能。
///
/// ## 谁是权威
///
/// **正文（SKILL.md 全文）是权威**，DTO 上的 `name` / `description` /
/// `keywords` 只是冗余镜像——宿主不解析正文也能列清单、做面板。所以合并
/// 规则是「正文缺什么补什么」，而不是「DTO 覆盖正文」：
///
/// | 字段 | 规则 |
/// |---|---|
/// | `name` | 正文 frontmatter 缺省时用 DTO（`parse_skill_text` 的兜底） |
/// | `description` / `keywords` / `category` / `package` | 正文为空才用 DTO |
/// | `always` / `system` | 逻辑或（`false` 既是"未声明"也是合法值，无法区分，取或最不会丢声明） |
fn into_skill(contribution: SkillContribution) -> Skill {
    let Skill {
        mut metadata,
        instructions,
    } = agent_module_skills::loader::parse_skill_text(&contribution.body, &contribution.name);
    if metadata.description.is_empty() {
        metadata.description = contribution.description;
    }
    if metadata.keywords.is_empty() {
        metadata.keywords = contribution.keywords;
    }
    if metadata.category.is_empty() {
        metadata.category = contribution.category;
    }
    if metadata.package.is_none() {
        metadata.package = contribution.package;
    }
    metadata.always |= contribution.always;
    metadata.system |= contribution.system;
    Skill {
        metadata,
        instructions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_skill_contribution_and_parses_body() {
        let contribution = SkillContribution {
            name: "qq-transport".into(),
            description: "QQ 消息收发".into(),
            keywords: vec!["qq".into()],
            always: true,
            system: false,
            category: "qq".into(),
            package: Some("echo-agent.instance.qq".into()),
            body: "---\nname: qq-transport\ndescription: 从正文覆盖\nkeywords: [qq, 消息]\n---\n# 正文\n".into(),
        }
        .into_custom();

        let decoded = skill_from_custom(&contribution).expect("skill kind");
        let skill = into_skill(decoded);
        // 正文里的 frontmatter 是权威：DTO 上的 name 只做兜底
        assert_eq!(skill.metadata.name, "qq-transport");
        assert_eq!(skill.metadata.description, "从正文覆盖");
        assert_eq!(skill.metadata.keywords, vec!["qq".to_string(), "消息".to_string()]);
        assert_eq!(skill.instructions, "# 正文");
    }

    #[test]
    fn bare_body_without_frontmatter_uses_dto_fields() {
        let dto = SkillContribution {
            name: "plain".into(),
            description: "无 frontmatter".into(),
            keywords: vec!["k".into()],
            always: false,
            system: true,
            category: String::new(),
            package: None,
            body: "只有正文。".into(),
        };
        let skill = into_skill(dto);
        assert_eq!(skill.metadata.name, "plain");
        assert_eq!(skill.instructions, "只有正文。");
    }
}

#[cfg(test)]
mod owner_tests {
    use super::*;

    fn skill(package: Option<&str>) -> Skill {
        let mut skill = Skill::direct("x", "d", Vec::new(), false, "c", "正文");
        skill.metadata.package = package.map(str::to_string);
        skill
    }

    /// 同一平台模块的多个实例（不同 module id）带同一份技能 → 归到一个
    /// owner，注册表不会来回抖。
    #[test]
    fn same_package_across_instances_yields_one_owner() {
        let skills = vec![skill(Some("echo-agent.instance.qq"))];
        assert_eq!(owner_of("instance-qq-qq", &skills), "echo-agent.instance.qq");
        assert_eq!(owner_of("instance-qq-qq-2", &skills), "echo-agent.instance.qq");
    }

    /// 没声明 package（内置模块的常见情形）→ 用 module id。
    #[test]
    fn missing_package_falls_back_to_module_id() {
        assert_eq!(owner_of("echo-agent.subagent", &[skill(None)]), "echo-agent.subagent");
        assert_eq!(owner_of("echo-agent.prodoc", &[]), "echo-agent.prodoc");
    }

    /// package 不一致 = 一个模块带多个包 → 只能按模块 id 归（拆开撤没意义：
    /// 模块要么在要么不在）。
    #[test]
    fn mixed_packages_fall_back_to_module_id() {
        let skills = vec![skill(Some("a")), skill(Some("b"))];
        assert_eq!(owner_of("m", &skills), "m");
    }
}
