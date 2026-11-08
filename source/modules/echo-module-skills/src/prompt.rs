//! 技能提示词贡献：技能选择、匹配和文本组装全部归 skills 模块。
//!
//! Agent 只通过通用 `ModuleTextBlock` 接口收集文本，不认识 Skill 类型。

pub use echo_defs::prompt::PromptBlock;

/// 为当前 turn 生成全部技能提示词块。
pub fn prompt_blocks(
    skills: &crate::SkillRegistry,
    content: &str,
    persona_system_skills: &[String],
) -> Vec<PromptBlock> {
    let mut blocks = Vec::new();
    let matched = skills.find_matching(content);

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
    if let Some(skill) = matched {
        blocks.push(PromptBlock {
            key: format!("triggered:{}", skill.metadata.name),
            label: format!("触发技能 · {}", skill.metadata.name),
            kind: "triggered".into(),
            content: format!(
                "# Triggered skill: {}\n{}",
                skill.metadata.name, skill.instructions
            ),
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
    let blocks = prompt_blocks(skills, content, persona_system_skills);
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
        let blocks = prompt_blocks(&skills, content, &persona);
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
