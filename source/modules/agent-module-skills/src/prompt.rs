//! 技能提示词贡献：技能选择、匹配和文本组装全部归 skills 模块。
//!
//! Agent 只通过通用 `ModuleTextBlock` 接口收集文本，不认识 Skill 类型。
//!
//! # 正文内联与索引（2026-11 用量修复）
//!
//! 旧行为是**所有** `always: true` 技能、人格系统技能与当轮触发技能的正文
//! 全文拼进系统提示。系统提示每步都随请求重发一遍，于是几十 KB 的技能
//! 正文会被重复计费 N 次（N = 一个 turn 里的模型步数）；实测本机
//! `long-task-efficiency` 单个 18.5 KB，四个常驻技能合计 24 KB。
//!
//! 现在分两档：
//!
//! - 正文 ≤ `inline_max_chars`（缺省 4000）：照旧全文内联——短技能（人格、
//!   对话风格）是行为准则，全文内联值得这点 token；
//! - 超过阈值：只内联**索引块**（名称 / description / 正文开头预览 / 总长度
//!   + `read_skill` 读取提示）。模型需要细节时用一次工具调用换完整正文，
//!   代价从"每步重发"降到"一次读取"。
//!
//! `inline_max_chars = 0` 表示回到旧行为（全部全文），用于排障。

pub use echo_defs::prompt::PromptBlock;

/// 索引模式下，索引块里额外内联的正文预览长度（字符）。
///
/// 为什么不是 0：正文开头通常就是最关键的判据（安全红线、何时该用这个
/// 技能）。给一段预览让模型**在不读全文时也能判断要不要读**，比只给一个
/// 名字少了整整一轮试错（试错的代价是这个 turn 多一次模型往返）。
pub const INDEX_PREVIEW_CHARS: usize = 600;

/// 缺省的正文内联上限（字符）。见模块文档的取舍。
pub const DEFAULT_INLINE_MAX_CHARS: usize = 4000;

/// 为一个技能生成"内联还是索引"的决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineDecision {
    /// 全文内联。
    Inline,
    /// 只给索引 + 预览，正文用 `read_skill` 读。
    Index,
}

/// 按正文长度决定内联方式。`inline_max_chars = 0` 恒为全文（兼容旧行为）。
pub fn inline_decision(instructions: &str, inline_max_chars: usize) -> InlineDecision {
    if inline_max_chars == 0 {
        return InlineDecision::Inline;
    }
    if instructions.chars().count() <= inline_max_chars {
        InlineDecision::Inline
    } else {
        InlineDecision::Index
    }
}

/// 生成索引块正文：名称、说明、开头预览、总长度与读取指引。
pub fn render_index_block(
    name: &str,
    description: &str,
    instructions: &str,
    preview_chars: usize,
) -> String {
    let total = instructions.chars().count();
    let preview: String = instructions.chars().take(preview_chars).collect();
    let mut out = format!("# Skill index: {name}\n");
    if !description.trim().is_empty() {
        out.push_str(&format!("{description}\n"));
    }
    out.push_str(&format!(
        "（正文 {total} 字符，未内联：内容超出内联上限。需要完整规则时调用 \
         read_skill(name=\"{name}\")，可用 offset / limit 分页。）\n"
    ));
    if !preview.trim().is_empty() {
        out.push_str("\n--- 正文开头 ---\n");
        out.push_str(&preview);
        if total > preview_chars {
            out.push_str("\n…（后略）");
        }
        out.push('\n');
    }
    out
}

/// 为当前 turn 生成全部技能提示词块（缺省内联上限）。
pub fn prompt_blocks(
    skills: &crate::SkillRegistry,
    content: &str,
    persona_system_skills: &[String],
) -> Vec<PromptBlock> {
    prompt_blocks_with(
        skills,
        content,
        persona_system_skills,
        DEFAULT_INLINE_MAX_CHARS,
    )
}

/// 同上，可指定正文内联上限（`0` = 全部全文）。
pub fn prompt_blocks_with(
    skills: &crate::SkillRegistry,
    content: &str,
    persona_system_skills: &[String],
    inline_max_chars: usize,
) -> Vec<PromptBlock> {
    let mut blocks = Vec::new();
    let matched = skills.find_matching(content);

    // 人格**显式声明**的系统技能：这是用户点名要常驻的行为准则，无论多长
    // 都全文内联——把它们索引化等于悄悄改掉人格的行为约束。
    let mut system_skills: Vec<&crate::Skill> = skills
        .all()
        .into_iter()
        .filter(|skill| skill.metadata.system && skill.metadata.enabled)
        .collect();
    system_skills.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
    for skill in system_skills {
        blocks.push(PromptBlock {
            key: format!("system:{}", skill.metadata.name),
            label: format!("系统提示词 · {}", skill.metadata.name),
            kind: "system-skill".into(),
            content: skill.instructions.clone(),
        });
    }

    let mut persona_skills: Vec<&crate::Skill> = skills
        .all()
        .into_iter()
        .filter(|skill| {
            persona_system_skills
                .iter()
                .any(|name| name == &skill.metadata.name)
        })
        .collect();
    persona_skills.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
    for skill in persona_skills {
        blocks.push(PromptBlock {
            key: format!("persona:{}", skill.metadata.name),
            label: format!("人格系统提示词 · {}", skill.metadata.name),
            kind: "system-skill".into(),
            content: skill.instructions.clone(),
        });
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
        let name = skill.metadata.name.clone();
        let content = match inline_decision(&skill.instructions, inline_max_chars) {
            InlineDecision::Inline => format!("# Active skill: {name}\n{}", skill.instructions),
            InlineDecision::Index => render_index_block(
                &name,
                &skill.metadata.description,
                &skill.instructions,
                INDEX_PREVIEW_CHARS,
            ),
        };
        blocks.push(PromptBlock {
            key: format!("skill:{name}"),
            label: format!("常驻技能 · {name}"),
            kind: "skill".into(),
            content,
        });
    }
    if let Some(skill) = matched {
        let name = skill.metadata.name.clone();
        let content = match inline_decision(&skill.instructions, inline_max_chars) {
            InlineDecision::Inline => {
                format!("# Triggered skill: {name}\n{}", skill.instructions)
            }
            InlineDecision::Index => {
                let index = render_index_block(
                    &name,
                    &skill.metadata.description,
                    &skill.instructions,
                    INDEX_PREVIEW_CHARS,
                );
                format!("# Triggered skill: {name}\n{index}")
            }
        };
        blocks.push(PromptBlock {
            key: format!("triggered:{name}"),
            label: format!("触发技能 · {name}"),
            kind: "triggered".into(),
            content,
        });
    }
    blocks
}

/// 通用模块文本接口只返回一个 JSON 块；Agent 负责展开为多个通用块。
pub fn prompt_json(
    skills: &crate::SkillRegistry,
    content: &str,
    persona_system_skills: &[String],
) -> Option<String> {
    prompt_json_with(
        skills,
        content,
        persona_system_skills,
        DEFAULT_INLINE_MAX_CHARS,
    )
}

/// 同上，可指定正文内联上限。
pub fn prompt_json_with(
    skills: &crate::SkillRegistry,
    content: &str,
    persona_system_skills: &[String],
    inline_max_chars: usize,
) -> Option<String> {
    let blocks = prompt_blocks_with(skills, content, persona_system_skills, inline_max_chars);
    if blocks.is_empty() {
        None
    } else {
        serde_json::to_string(&blocks).ok()
    }
}

/// skills 模块的通用刷新器：Agent 只提供自由 JSON，不接触技能概念。
pub struct SkillPromptRefresher {
    pub registry: std::sync::Arc<tokio::sync::Mutex<crate::SkillRegistry>>,
    pub store: echo_defs::prompt::SharedPromptStore,
}

impl echo_defs::prompt::PromptRefresher for SkillPromptRefresher {
    fn refresh(&self, context: &serde_json::Value) {
        let Ok(skills) = self.registry.try_lock() else {
            return;
        };
        let content = context
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let persona: Vec<String> = context
            .get("persona_system_skills")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        // 宿主可经上下文传内联上限（`[agent] skill_inline_max_chars`）。
        let inline_max = context
            .get("skill_inline_max_chars")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_INLINE_MAX_CHARS);
        let blocks = prompt_blocks_with(&skills, content, &persona, inline_max);
        self.store.replace("skills", blocks);
        self.store.invalidate();
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct TestPromptStore;
#[cfg(test)]
impl echo_defs::prompt::PromptStore for TestPromptStore {
    fn replace(&self, _: &str, _: Vec<echo_defs::prompt::PromptBlock>) {}
    fn remove(&self, _: &str) {}
    fn invalidate(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, always: bool, body: &str) -> crate::Skill {
        crate::Skill {
            metadata: crate::SkillMetadata {
                name: name.into(),
                description: "一句说明".into(),
                keywords: vec![],
                always,
                system: false,
                enabled: true,
                category: String::new(),
                package: None,
            },
            instructions: body.into(),
        }
    }

    fn registry_with(skills: Vec<crate::Skill>) -> crate::SkillRegistry {
        let mut registry = crate::SkillRegistry::new();
        for skill in skills {
            registry.register(skill);
        }
        registry
    }

    /// 短技能照旧全文内联（人格/对话风格属于这一类）。
    #[test]
    fn short_skill_is_inlined_in_full() {
        let registry = registry_with(vec![skill("short", true, "短正文")]);
        let blocks = prompt_blocks_with(&registry, "", &[], 100);
        let block = blocks
            .iter()
            .find(|b| b.key == "skill:short")
            .expect("block");
        assert!(block.content.contains("短正文"));
        assert!(!block.content.contains("read_skill"));
    }

    /// 长技能只给索引：正文不出现，但名称、长度与读取指引必须给出。
    #[test]
    fn long_skill_becomes_index_with_preview() {
        let body = "红线规则\n".to_string() + &"x".repeat(5000);
        let registry = registry_with(vec![skill("long", true, &body)]);
        let blocks = prompt_blocks_with(&registry, "", &[], 1000);
        let block = blocks
            .iter()
            .find(|b| b.key == "skill:long")
            .expect("block");
        assert!(block.content.contains("Skill index: long"), "{}", block.content);
        assert!(block.content.contains("read_skill"), "读取指引缺失");
        assert!(block.content.contains("红线规则"), "预览必须带头部判据");
        // 正文远超 1000 字符，索引块必须明显小于正文。
        assert!(
            block.content.chars().count() < body.chars().count() / 2,
            "index block too large: {} chars",
            block.content.chars().count()
        );
    }

    /// `inline_max_chars = 0` = 回到旧行为（排障开关）。
    #[test]
    fn zero_limit_restores_full_inline() {
        let body = "y".repeat(50_000);
        let registry = registry_with(vec![skill("huge", true, &body)]);
        let blocks = prompt_blocks_with(&registry, "", &[], 0);
        let block = blocks
            .iter()
            .find(|b| b.key == "skill:huge")
            .expect("block");
        assert_eq!(block.content.chars().count(), body.chars().count() + "# Active skill: huge\n".len());
    }

    /// 人格显式声明的系统技能永不被索引化（那是用户点名的行为准则）。
    #[test]
    fn persona_system_skills_stay_inline_regardless_of_size() {
        let body = "z".repeat(20_000);
        let registry = registry_with(vec![skill("strict", false, &body)]);
        let blocks = prompt_blocks_with(&registry, "", &["strict".to_string()], 1000);
        let block = blocks
            .iter()
            .find(|b| b.key == "persona:strict")
            .expect("block");
        assert_eq!(block.content.chars().count(), 20_000);
    }
}
