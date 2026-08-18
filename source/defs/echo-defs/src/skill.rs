//! The skill seam — vocabulary types and the provider trait.
//!
//! A skill is a SKILL.md-driven capability with progressive disclosure:
//! Tier-1 metadata (name + description + keywords) is always cheap to load
//! into the system prompt; Tier-2 full instructions load when the skill
//! triggers. The concrete file discovery/loader lives in a provider crate,
//! never here.

/// Skill metadata (Tier 1 — cheap, always loaded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    /// Keywords that trigger this skill when they appear in a message.
    pub keywords: Vec<String>,
    /// Whether this skill is included in every conversation.
    pub always: bool,
    pub enabled: bool,
}

/// A loaded skill definition (Tier 2 — full instructions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub metadata: SkillMetadata,
    pub instructions: String,
}

impl Skill {
    /// Whether a message triggers this skill (keyword match, case-insensitive).
    pub fn matches(&self, content: &str) -> bool {
        if !self.metadata.enabled {
            return false;
        }
        let lower = content.to_lowercase();
        self.metadata
            .keywords
            .iter()
            .any(|k| !k.is_empty() && lower.contains(&k.to_lowercase()))
    }
}

/// The skill-provider seam: a registry of discovered skills.
///
/// Implementations own discovery, loading, and hot-reload; consumers
/// (prompt assembly, the skill tools) depend only on this trait.
pub trait SkillProvider: Send + Sync {
    /// Look up a skill by name.
    fn get(&self, name: &str) -> Option<&Skill>;
    /// The first enabled skill triggered by the message (deterministic order).
    fn find_matching(&self, content: &str) -> Option<&Skill>;
    /// Enabled skills whose instructions apply to every conversation.
    fn always_enabled(&self) -> Vec<&Skill>;
    /// Enable/disable a skill by name; false when unknown.
    fn set_enabled(&mut self, name: &str, enabled: bool) -> bool;
    /// All skill names.
    fn names(&self) -> Vec<String>;
    /// Enabled skill names.
    fn enabled_names(&self) -> Vec<String>;
}
