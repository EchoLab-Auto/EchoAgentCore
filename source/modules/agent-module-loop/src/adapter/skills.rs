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
        let skills = self
            .contributions()
            .into_iter()
            .filter_map(|contribution| skill_from_custom(&contribution).map(into_skill))
            .collect();
        (self.module_id().to_string(), skills)
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
