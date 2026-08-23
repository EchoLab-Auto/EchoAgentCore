//! Skill system: SKILL.md-driven capabilities with progressive disclosure.
//!
//! The `Skill`/`SkillMetadata` vocabulary and the `SkillProvider` seam live
//! in [`echo_defs`](echo_defs); this module re-exports them (keeping the
//! `echo_agent::skill::…` paths) and owns the concrete [`SkillRegistry`]
//! (file discovery + hot-reload state).

pub use echo_defs::skill::{Skill, SkillMetadata};

pub mod loader;

use std::collections::HashMap;

use thiserror::Error;

/// Registry of discovered skills.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Discover skills from `dir` (walks recursively for `SKILL.md` files).
    pub fn discover(dir: &str) -> Result<Self, SkillError> {
        let mut registry = Self::new();
        if dir.is_empty() || !std::path::Path::new(dir).exists() {
            return Ok(registry);
        }
        for entry in walkdir::WalkDir::new(dir) {
            let entry = entry.map_err(|error| SkillError::Load {
                path: error
                    .path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| dir.to_string()),
                err: error.to_string(),
            })?;
            let path = entry.path();
            if path.is_file() && path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
                let skill = loader::load_skill(path)?;
                registry.register(skill);
            }
        }
        Ok(registry)
    }

    pub fn register(&mut self, skill: Skill) {
        self.skills.insert(skill.metadata.name.clone(), skill);
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// Find the first enabled skill triggered by the message.
    /// HashMap 迭代顺序随机，命中多个时按名称排序取第一个，保证确定性。
    pub fn find_matching(&self, content: &str) -> Option<&Skill> {
        let mut matched: Vec<&Skill> = self
            .skills
            .values()
            .filter(|s| !s.metadata.always && s.matches(content))
            .collect();
        matched.sort_by_key(|s| s.metadata.name.clone());
        matched.into_iter().next()
    }

    /// Enabled skills whose instructions apply to every conversation.
    pub fn always_enabled(&self) -> Vec<&Skill> {
        let mut skills: Vec<&Skill> = self
            .skills
            .values()
            .filter(|skill| skill.metadata.enabled && skill.metadata.always)
            .collect();
        skills.sort_by_key(|skill| skill.metadata.name.clone());
        skills
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        match self.skills.get_mut(name) {
            Some(s) => {
                s.metadata.enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// Preserve runtime enable/disable choices across a filesystem reload.
    /// Newly discovered skills keep their loader default (`enabled = true`).
    pub fn inherit_enabled_from(&mut self, previous: &Self) {
        for (name, skill) in &mut self.skills {
            if let Some(old) = previous.skills.get(name) {
                skill.metadata.enabled = old.metadata.enabled;
            }
        }
    }

    /// Compact Tier-1 listing for the system prompt.
    pub fn metadata_lines(&self) -> String {
        let mut lines: Vec<String> = self
            .skills
            .values()
            .filter(|s| s.metadata.enabled)
            .map(|s| {
                let keywords = if s.metadata.keywords.is_empty() {
                    String::new()
                } else {
                    format!(" (触发词: {})", s.metadata.keywords.join("、"))
                };
                let always = if s.metadata.always { " (常驻)" } else { "" };
                format!(
                    "- {}: {}{}{}",
                    s.metadata.name, s.metadata.description, always, keywords
                )
            })
            .collect();
        lines.sort();
        lines.join("\n")
    }

    /// All skills (name-sorted) for panel/API listings.
    pub fn all(&self) -> Vec<&Skill> {
        let mut list: Vec<&Skill> = self.skills.values().collect();
        list.sort_by_key(|s| s.metadata.name.clone());
        list
    }

    pub fn names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }

    pub fn enabled_names(&self) -> Vec<String> {
        self.skills
            .values()
            .filter(|s| s.metadata.enabled)
            .map(|s| s.metadata.name.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("SKILL.md 加载失败 {path}: {err}")]
    Load { path: String, err: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, keywords: &[&str]) -> Skill {
        Skill {
            metadata: SkillMetadata {
                name: name.into(),
                description: "desc".into(),
                keywords: keywords.iter().map(|k| k.to_string()).collect(),
                always: false,
                enabled: true,
            },
            instructions: "instructions".into(),
        }
    }

    #[test]
    fn keyword_matching() {
        let s = skill("web-search", &["天气", "新闻"]);
        assert!(s.matches("今天天气怎么样"));
        assert!(!s.matches("你好"));
    }

    #[test]
    fn registry_find_matching() {
        let mut reg = SkillRegistry::new();
        reg.register(skill("calc", &["计算"]));
        reg.register(skill("web", &["搜索"]));
        assert_eq!(
            reg.find_matching("帮我计算一下").unwrap().metadata.name,
            "calc"
        );
        assert!(reg.find_matching("随便聊聊").is_none());
    }

    #[test]
    fn disabled_skills_do_not_match() {
        let mut reg = SkillRegistry::new();
        let mut s = skill("calc", &["计算"]);
        s.metadata.enabled = false;
        reg.register(s);
        assert!(reg.find_matching("计算一下").is_none());
    }

    #[test]
    fn keyword_matching_is_case_insensitive() {
        let s = skill("calc", &["calc"]);
        assert!(s.matches("use Calc now"));
        assert!(s.matches("CALC"));
    }

    #[test]
    fn empty_keywords_never_match() {
        let s = skill("empty", &[""]);
        assert!(!s.matches("anything"));
        let s2 = skill("none", &[]);
        assert!(!s2.matches("anything"));
    }

    #[test]
    fn find_matching_is_deterministic_with_multiple_hits() {
        let mut reg = SkillRegistry::new();
        reg.register(skill("zeta", &["触发"]));
        reg.register(skill("alpha", &["触发"]));
        reg.register(skill("middle", &["触发"]));
        // Alphabetically first match wins, regardless of insertion order.
        assert_eq!(
            reg.find_matching("触发消息").unwrap().metadata.name,
            "alpha"
        );
    }

    #[test]
    fn always_enabled_is_sorted_and_ignores_disabled_skills() {
        let mut reg = SkillRegistry::new();
        let mut zeta = skill("zeta", &[]);
        zeta.metadata.always = true;
        reg.register(zeta);
        let mut alpha = skill("alpha", &[]);
        alpha.metadata.always = true;
        reg.register(alpha);
        let mut disabled = skill("disabled", &[]);
        disabled.metadata.always = true;
        disabled.metadata.enabled = false;
        reg.register(disabled);

        let names: Vec<&str> = reg
            .always_enabled()
            .iter()
            .map(|skill| skill.metadata.name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    #[test]
    fn always_skill_is_not_returned_as_keyword_match() {
        let mut reg = SkillRegistry::new();
        let mut always = skill("always", &["触发"]);
        always.metadata.always = true;
        reg.register(always);

        assert!(reg.find_matching("触发").is_none());
        assert_eq!(reg.always_enabled().len(), 1);
    }

    #[test]
    fn reload_inherits_enabled_state_but_keeps_new_content() {
        let mut previous = SkillRegistry::new();
        let mut old = skill("calc", &["old"]);
        old.metadata.enabled = false;
        previous.register(old);

        let mut reloaded = SkillRegistry::new();
        let mut updated = skill("calc", &["new"]);
        updated.instructions = "updated instructions".into();
        reloaded.register(updated);
        reloaded.register(skill("new-skill", &["new"]));
        reloaded.inherit_enabled_from(&previous);

        let calc = reloaded.get("calc").unwrap();
        assert!(!calc.metadata.enabled);
        assert_eq!(calc.metadata.keywords, vec!["new"]);
        assert_eq!(calc.instructions, "updated instructions");
        assert!(reloaded.get("new-skill").unwrap().metadata.enabled);
    }

    #[test]
    fn discover_walks_directories_recursively() {
        let dir = std::env::temp_dir().join(format!("echo-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: top-skill\ndescription: d\nkeywords: [top]\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            dir.join("nested").join("SKILL.md"),
            "---\nname: nested-skill\ndescription: d\nkeywords: [nest]\n---\nbody",
        )
        .unwrap();
        // An unrelated file must be ignored.
        std::fs::write(dir.join("README.md"), "# not a skill").unwrap();

        let reg = SkillRegistry::discover(dir.to_str().unwrap()).unwrap();
        assert!(reg.get("top-skill").is_some());
        assert!(reg.get("nested-skill").is_some(), "recursive discovery");
        assert_eq!(reg.get("top-skill").unwrap().instructions, "body");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_missing_or_empty_dir_is_empty() {
        assert!(SkillRegistry::discover("").unwrap().is_empty());
        let missing = std::env::temp_dir().join("echo-no-such-dir-xyz");
        assert!(SkillRegistry::discover(missing.to_str().unwrap())
            .unwrap()
            .is_empty());
    }
}
