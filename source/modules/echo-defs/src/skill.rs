//! The skill seam — vocabulary types and the provider trait.
//!
//! A skill is a SKILL.md-driven capability with progressive disclosure:
//! Tier-1 metadata (name + description + keywords) is always cheap to load
//! into the system prompt; Tier-2 full instructions load when the skill
//! triggers. The concrete file discovery/loader lives in a provider crate,
//! never here.
//!
//! 两条接缝同住这里，方向相反：
//!
//! - **发现**（`skills.dir` 模块自己做）：SKILL.md 目录 → 技能表；
//! - **注册**（[`SkillRegistrar`]）：模块在自己的 `mount` 里交出正文。
//!   归属跟着模块的生存期走，宿主不再持有一张"谁有哪些技能"的清单。

use std::sync::Arc;

use echo_context::ServiceKey;
/// Skill metadata (Tier 1 — cheap, always loaded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    /// Keywords that trigger this skill when they appear in a message.
    pub keywords: Vec<String>,
    /// Whether this skill is included in every conversation.
    pub always: bool,
    /// 系统提示词 skill：内容注入 base 区（身份/规则）。任意 SKILL.md
    /// 声明 `system: true` 即成为可插拔系统提示词的一部分。
    pub system: bool,
    pub enabled: bool,
    /// UI grouping category (empty = "未分类" on the frontend).
    pub category: String,
    /// Owning package id (from SKILL.md frontmatter `package:`); None = standalone.
    pub package: Option<String>,
}

/// A loaded skill definition (Tier 2 — full instructions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub metadata: SkillMetadata,
    pub instructions: String,
}

impl Skill {
    /// Build a skill directly from parts (used by builtin skill modules and
    /// tests; file-based discovery goes through the loader instead).
    pub fn direct(
        name: impl Into<String>,
        description: impl Into<String>,
        keywords: Vec<String>,
        always: bool,
        category: impl Into<String>,
        instructions: impl Into<String>,
    ) -> Self {
        Self {
            metadata: SkillMetadata {
                name: name.into(),
                description: description.into(),
                keywords,
                always,
                system: false,
                enabled: true,
                category: category.into(),
                package: None,
            },
            instructions: instructions.into(),
        }
    }

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

/// 技能**主动注册**接缝：模块在自己的 `mount` 里把技能正文交出去。
///
/// ## 为什么是"推"而不是"拉"
///
/// 在这条接缝之前，技能归属是宿主的硬编码清单
/// （`builtin_modules::skill_contributions()` 里逐条写死
/// `subagent` / `prodoc` 各带哪个技能）。那有两处结构性错误：
///
/// 1. **模块被禁用，技能还在**——清单不看挂载状态，宿主照旧把正文塞进
///    技能表，于是提示词里教着一个本部署根本没有的工具；
/// 2. **新增模块要改 Core**——正文属于模块，归属却写在宿主里，两份信息
///    必然漂移（改一处忘另一处）。
///
/// 现在是模块自己在 `mount` 里交正文：没挂载就没有技能，卸载时
/// [`SkillRegistrar::unregister`] 撤销——归属与生存期都由模块说话。
///
/// ## 为什么交正文而不是结构化 `Skill`
///
/// `Skill` 的解析（frontmatter / 关键词 / `system:` 标记）是技能模块的
/// 领域知识，本层只认词汇。模块只交出「正文 + 兜底名」，解析在
/// [`SkillRegistrar`] 的实现侧完成——模块因此不必依赖解析器，也不会
/// 出现两份格式定义。
///
/// 键名是**跨 crate 的字符串契约**（`echo-agent.host.skill_registrar`）：
/// 提供方是技能模块，消费方是任意 `kind = "tool"/"skill"` 的内置或外部
/// 模块。键与 trait 同住定义层，两侧只引用它，不各自写字面量。
pub trait SkillRegistrar: Send + Sync {
    /// 用 `owner`（模块 id）**整体替换**它贡献的技能集。
    ///
    /// 替换而不是追加：模块重启后上报的是它当前的完整清单，上一轮有、
    /// 这一轮没有的技能必须消失——追加会让已下线的技能永远留在提示词里。
    ///
    /// `skills` 是 `(SKILL.md 全文, frontmatter 缺失时的兜底名)`。解析失败
    /// 的条目跳过并记 warn（一个模块的坏文件不该让整张技能表消失），
    /// 只有"整体不可用"才返回 `Err`。
    fn register_markdown(&self, owner: &str, skills: &[(&str, &str)]) -> Result<(), String>;

    /// 撤销某个 `owner` 的全部技能（模块卸载 / 被禁用时由 disposer 调用）。
    ///
    /// 不存在的 `owner` 是空操作——卸载路径可以无脑调用。
    fn unregister(&self, owner: &str);
}

/// 服务键：技能注册接缝（提供方 = `skills.dir` 模块，消费方 = 任意模块）。
///
/// 与 [`SkillRegistrar`] 同住定义层：两侧都引用这一个常量，谁都不写第二份
/// 字面量——键名漂移的表现是"注册静默失效"，只有一份才防得住。
pub static SKILL_REGISTRAR: ServiceKey<Arc<dyn SkillRegistrar>> =
    ServiceKey::new("echo-agent.host.skill_registrar");
