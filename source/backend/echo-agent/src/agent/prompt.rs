//! System prompt construction: named blocks for token-usage visualization.
//!
//! Builds the system prompt from layers (base, system skills, persona skills,
//! skill list, always-on skills, triggered skills, boundary rules) as named
//! blocks, then joins them for the LLM request.

#[allow(dead_code)]
use crate::skill::SkillRegistry;

use super::boundary::{BoundaryKind, PromptBlock};

/// Build the system prompt as named blocks so the panel can visualize
/// token usage per section.
pub(crate) async fn build_prompt_blocks(
    skills: &SkillRegistry,
    base_prompt: &str,
    content: &str,
    boundary: Option<BoundaryKind>,
    persona_system_skills: &[String],
) -> Vec<PromptBlock> {
    let matched = skills.find_matching(content);
    let mut blocks = Vec::new();
    blocks.push(PromptBlock {
        key: "base".into(),
        label: "系统提示词".into(),
        kind: "base".into(),
        content: base_prompt.to_string(),
    });
    // 可插拔系统提示词：所有启用的 system:true skill 注入 base 区
    //（身份/规则层，任意 SKILL.md 声明 system: true 即参与）。
    {
        let mut system_skills: Vec<&crate::skill::Skill> = skills
            .all()
            .into_iter()
            .filter(|sk| sk.metadata.system && sk.metadata.enabled)
            .collect();
        system_skills.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
        for sk in system_skills {
            blocks.push(PromptBlock {
                key: format!("system:{}", sk.metadata.name),
                label: format!("系统提示词 · {}", sk.metadata.name),
                kind: "system-skill".into(),
                content: sk.instructions.clone(),
            });
        }
    }
    // persona 级系统提示词 skill（TeamMember.system_skills 引用，
    // 非空时作为该 agent 的额外身份层，追加在全局 system skills 后）。
    if !persona_system_skills.is_empty() {
        let mut persona_skills: Vec<&crate::skill::Skill> = skills
            .all()
            .into_iter()
            .filter(|sk| persona_system_skills.iter().any(|n| n == &sk.metadata.name))
            .collect();
        persona_skills.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
        for sk in persona_skills {
            blocks.push(PromptBlock {
                key: format!("persona:{}", sk.metadata.name),
                label: format!("人格系统提示词 · {}", sk.metadata.name),
                kind: "system-skill".into(),
                content: sk.instructions.clone(),
            });
        }
    }
    if !skills.is_empty() {
        blocks.push(PromptBlock {
            key: "skills".into(),
            label: "技能清单".into(),
            kind: "skills".into(),
            content: format!("# Available skills
{}", skills.metadata_lines()),
        });
    }
    for skill in skills.always_enabled() {
        blocks.push(PromptBlock {
            key: format!("skill:{}", skill.metadata.name),
            label: format!("常驻技能 · {}", skill.metadata.name),
            kind: "skill".into(),
            content: format!(
                "# Active skill: {}
{}",
                skill.metadata.name, skill.instructions
            ),
        });
    }
    if let Some(matched) = matched {
        blocks.push(PromptBlock {
            key: format!("triggered:{}", matched.metadata.name),
            label: format!("触发技能 · {}", matched.metadata.name),
            kind: "triggered".into(),
            content: format!(
                "# Triggered skill: {}
{}",
                matched.metadata.name, matched.instructions
            ),
        });
    }
    if let Some(boundary) = boundary {
        blocks.push(boundary.block());
    }
    blocks
}

/// Join prompt blocks with the same separator the original string builder used.
pub(crate) fn join_prompt_blocks(blocks: &[PromptBlock]) -> String {
    blocks
        .iter()
        .map(|block| block.content.as_str())
        .collect::<Vec<_>>()
        .join("

")
}
