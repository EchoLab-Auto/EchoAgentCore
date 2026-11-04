//! System prompt construction: named blocks for token-usage visualization.
//!
//! Builds the system prompt from layers (base, system skills, persona skills,
//! skill list, always-on skills, triggered skills, workspace, boundary rules)
//! as named blocks, then joins them for the LLM request.

use crate::skill::SkillRegistry;

use super::boundary::{BoundaryKind, PromptBlock};

/// Build the system prompt as named blocks so the panel can visualize
/// token usage per section.
///
/// `plugin_blocks` 是**已挂载插件贡献的有序文本块**（
/// `Contribution::Text`；由 `PluginHost::collect_text_blocks` 收集，已按 order
/// 排好序、已丢弃空内容）。
///
/// 为什么不再是 `workspace: Option<String>`：那是"某个插件专属内容"作为位置
/// 参数写进装配函数——每来一个需要注入内容的插件就要改一次签名。现在装配与
/// 插件数量解耦：插件在不在、有几个，这个函数都不用改。
///
/// 插件的块的 `kind` 取它的 `key`（如 `workspace`），所以面板的种类徽标与
/// 排序语义保持不变。
pub(crate) async fn build_prompt_blocks(
    skills: &SkillRegistry,
    base_prompt: &str,
    content: &str,
    boundary: Option<BoundaryKind>,
    persona_system_skills: &[String],
    plugin_blocks: &[(echo_plugin::PluginTextBlock, String)],
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
            content: format!("# Available skills\n{}", skills.metadata_lines()),
        });
    }
    for skill in skills.always_enabled() {
        blocks.push(PromptBlock {
            key: format!("skill:{}", skill.metadata.name),
            label: format!("常驻技能 · {}", skill.metadata.name),
            kind: "skill".into(),
            content: format!(
                "# Active skill: {}\n{}",
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
                "# Triggered skill: {}\n{}",
                matched.metadata.name, matched.instructions
            ),
        });
    }
    // 插件贡献的文本块（`Contribution::Text`）：按 order 排好序、空内容已丢。
    //
    // 位置在"触发技能"之后、"边界"之前——与历史行为一致（工作区会话提示词
    // 原本就在这里）。`kind` 取块的 `key`，面板的种类徽标因此不变。
    for (block, text) in plugin_blocks {
        blocks.push(PromptBlock {
            key: block.key.clone(),
            label: if block.label.is_empty() {
                block.key.clone()
            } else {
                block.label.clone()
            },
            kind: block.key.clone(),
            content: text.clone(),
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
        .join("\n\n")
}
