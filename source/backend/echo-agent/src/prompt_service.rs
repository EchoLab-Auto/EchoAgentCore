//! 提示词服务（S4c）：技能注册表 + 系统提示词缓存 + 最近一次装配结果。
//!
//! ## 为什么这三样归一起
//!
//! 它们描述的是**同一件事的三个时刻**：
//!
//! | 字段 | 是什么 |
//! |---|---|
//! | `skills` | 输入的素材（用户/内置 SKILL.md，可热重载） |
//! | `system_prompt_cache` | 装配结果的缓存（技能/人格/边界变了就失效） |
//! | `last_prompt_blocks` | 上一次真正发给模型的**按会话**快照（`/context` 视图） |
//!
//! 改造前它们散在 `Agent` 上，而"什么时候该失效缓存"这条知识散在 4 个调用点
//! （`apply_capabilities` / `reload_skills` / `SetSystemPrompt` / 技能启停）。
//! 收进服务后，失效与装配在同一处——加一条新的失效触发条件时只要看这个文件。
//!
//! ## 缓存失效为什么不是自动的
//!
//! 装配的输入不止技能：persona 的 `system_prompt`、边界规则、插件贡献的文本块
//! 都能变，而它们各自有独立的变更路径。**自动失效需要在每个输入上挂钩子**，
//! 那比"显式 `invalidate()`"更容易漏而且更难查（漏钩子 = 用了旧缓存，静默）。
//! 这里选显式：调用方知道自己改了什么，改完调一次。
//!
//! 缓存本身只缓存**无输入相关区块**的那一份（`build` 的 `content` 为空时），
//! 也就是 `/context` 在没有最近 turn 时回退用的那个代表值。

use std::sync::Arc;

use crate::agent::boundary::{BoundaryKind, PromptBlock};
use crate::skill::SkillRegistry;

/// 提示词装配服务（见模块文档）。
pub(crate) struct PromptService {
    /// 技能注册表（可热重载：用户目录 + 内置层）。
    pub(crate) skills: Arc<tokio::sync::Mutex<SkillRegistry>>,
    /// 无输入相关区块的**系统提示词字符串**缓存（`None` = 待重算）。
    ///
    /// 存字符串而不是块：当前唯一的读者是 `build_system_prompt`（把块拼成
    /// 一段文本给模型）。块级快照另有 [`Self::last_prompt_blocks`]——那是
    /// `/context` 视图要的**按会话**数据，与这个"与输入无关的缓存"不是一回事。
    system_prompt_cache: tokio::sync::RwLock<Option<String>>,
    /// 上一次 turn 的装配结果，**按会话**（`/context` 视图用）。
    ///
    /// 为什么按会话：`/context` 看的是"我这个会话上一轮发了什么"。单槽会让
    /// 会话 A 的 turn 覆盖会话 B 的视图（2026-10 修过的真实 bug）。
    last_prompt_blocks: dashmap::DashMap<String, Vec<PromptBlock>>,
}

impl PromptService {
    /// 用技能注册表构造。
    pub(crate) fn new(skills: Arc<tokio::sync::Mutex<SkillRegistry>>) -> Self {
        Self {
            skills,
            system_prompt_cache: tokio::sync::RwLock::new(None),
            last_prompt_blocks: dashmap::DashMap::new(),
        }
    }

    /// 让缓存失效（技能 / 人格提示词 / 边界发生变化后调）。
    ///
    /// **不清 `last_prompt_blocks`**：那是"已经发出去的东西"的历史快照，
    /// 清掉会让 `/context` 在下一轮之前变成空的（用户刚改完设置就看到空白页）。
    pub(crate) async fn invalidate(&self) {
        *self.system_prompt_cache.write().await = None;
    }

    /// 记下某个会话上一轮的装配结果（`/context` 视图数据源）。
    pub(crate) fn remember(&self, session_id: &str, blocks: Vec<PromptBlock>) {
        self.last_prompt_blocks
            .insert(session_id.to_string(), blocks);
    }

    /// 某会话上一轮的装配结果（`None` = 还没跑过 turn）。
    pub(crate) fn last_blocks(&self, session_id: &str) -> Option<Vec<PromptBlock>> {
        self.last_prompt_blocks
            .get(session_id)
            .map(|entry| entry.value().clone())
    }

    /// 缓存的系统提示词（`None` = 未缓存 / 已失效）。
    ///
    /// 当前唯一读者是 `build_system_prompt`（`#[cfg(test)]`）——它保留着
    /// 旧的"拼成一整段字符串"语义供测试对照。非测试构建里是死代码。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn cached_prompt(&self) -> Option<String> {
        self.system_prompt_cache.read().await.clone()
    }

    /// 写入缓存（调用方刚算出一份"与输入无关"的提示词时）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn cache_prompt(&self, prompt: String) {
        *self.system_prompt_cache.write().await = Some(prompt);
    }

    /// 装配系统提示词块（**纯装配**：素材由调用方按需提供）。
    ///
    /// 与自由函数 [`crate::agent::prompt::build_prompt_blocks`] 的分工：那个是
    /// 纯函数（便于单测），本方法是它在服务上的入口——锁技能表、调纯函数、
    /// 返回结果。**不做缓存**（带 `content` 的装配与输入强相关，缓存无意义）。
    #[allow(dead_code)] // 预留：装配入口迁到服务（下一步把 turn.rs 的调用改过来）
    pub(crate) async fn assemble(
        &self,
        base_prompt: &str,
        content: &str,
        boundary: Option<BoundaryKind>,
        persona_system_skills: &[String],
        plugin_blocks: &[(echo_plugin::PluginTextBlock, String)],
    ) -> Vec<PromptBlock> {
        let skills = self.skills.lock().await;
        crate::agent::prompt::build_prompt_blocks(
            &skills,
            base_prompt,
            content,
            boundary,
            persona_system_skills,
            plugin_blocks,
        )
        .await
    }
}
