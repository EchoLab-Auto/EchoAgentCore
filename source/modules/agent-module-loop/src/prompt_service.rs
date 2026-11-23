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
//! 装配的输入不止技能：persona 的 `system_prompt`、边界规则、模块贡献的文本块
//! 都能变，而它们各自有独立的变更路径。**自动失效需要在每个输入上挂钩子**，
//! 那比"显式 `invalidate()`"更容易漏而且更难查（漏钩子 = 用了旧缓存，静默）。
//! 这里选显式：调用方知道自己改了什么，改完调一次。
//!
//! 缓存本身只缓存**无输入相关区块**的那一份（`build` 的 `content` 为空时），
//! 也就是 `/context` 在没有最近 turn 时回退用的那个代表值。

use std::sync::Arc;

use crate::agent::boundary::PromptBlock;
use std::collections::BTreeMap;

// `skills` 字段只在 `#[cfg(test)]` 下存在（给旧单元测试注入技能用），所以这个导入
// 也只在测试构建里需要。无条件导入会被判 unused（`cargo clippy --fix` 删过一版，
// 导致 lib test 里 `PromptService::new` 找不到 `SkillRegistry`）。
#[cfg(test)]
use crate::skill::SkillRegistry;

/// 提示词装配服务（见模块文档）。
pub(crate) struct PromptService {
    /// 仅供旧单元测试注入初始技能；生产构建没有技能注册表字段。
    #[cfg(test)]
    pub(crate) skills: Arc<tokio::sync::Mutex<SkillRegistry>>,
    /// 无输入相关区块的**系统提示词字符串**缓存（`None` = 待重算）。
    ///
    /// 存字符串而不是块：这个槽服务的是"把块拼成一段文本"的旧口径（唯一读者
    /// 曾是 `#[cfg(test)]` 的 `build_system_prompt`，现已直接读块），宿主侧目前
    /// 只写不读——保留它是因为**失效点清单**需要一个落点（`invalidate()` 的
    /// 调用点就是"什么输入变了"的清单）。块级快照另有
    /// [`Self::last_prompt_blocks`]：那是 `/context` 视图要的**按会话**数据，
    /// 与这个"与输入无关的缓存"不是一回事。
    system_prompt_cache: tokio::sync::RwLock<Option<String>>,
    /// 上一次 turn 的装配结果，**按会话**（`/context` 视图用）。
    ///
    /// 为什么按会话：`/context` 看的是"我这个会话上一轮发了什么"。单槽会让
    /// 会话 A 的 turn 覆盖会话 B 的视图（2026-10 修过的真实 bug）。
    last_prompt_blocks: dashmap::DashMap<String, Vec<PromptBlock>>,
    /// 模块主动写入的通用提示词块，按 owner 隔离。
    contributed: std::sync::RwLock<BTreeMap<String, Vec<echo_defs::prompt::PromptBlock>>>,
    pub(crate) refresher: std::sync::RwLock<Option<echo_defs::prompt::SharedPromptRefresher>>,
    capability_controller:
        std::sync::RwLock<Option<Arc<dyn echo_defs::prompt::NamedCapabilityController>>>,
}

impl PromptService {
    /// 用技能注册表构造。
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(test)]
            skills: Arc::new(tokio::sync::Mutex::new(SkillRegistry::new())),
            system_prompt_cache: tokio::sync::RwLock::new(None),
            last_prompt_blocks: dashmap::DashMap::new(),
            contributed: std::sync::RwLock::new(BTreeMap::new()),
            refresher: std::sync::RwLock::new(None),
            capability_controller: std::sync::RwLock::new(None),
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
}

impl PromptService {
    pub(crate) fn set_refresher(&self, refresher: echo_defs::prompt::SharedPromptRefresher) {
        *self
            .refresher
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(refresher);
    }

    pub(crate) fn set_capability_controller(
        &self,
        controller: Arc<dyn echo_defs::prompt::NamedCapabilityController>,
    ) {
        *self
            .capability_controller
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(controller);
    }

    pub(crate) async fn apply_named_capabilities(
        &self,
        enabled: Vec<String>,
        disabled: Vec<String>,
        global_disabled: Vec<String>,
    ) {
        let controller = self
            .capability_controller
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(controller) = controller {
            controller.apply(enabled, disabled, global_disabled).await;
        }
    }

    pub(crate) fn capability_controller(
        &self,
    ) -> Option<Arc<dyn echo_defs::prompt::NamedCapabilityController>> {
        self.capability_controller
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn contributed_blocks(&self) -> Vec<PromptBlock> {
        self.contributed
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .flatten()
            .map(|b| PromptBlock {
                key: b.key.clone(),
                label: b.label.clone(),
                kind: b.kind.clone(),
                content: b.content.clone(),
            })
            .collect()
    }
}

impl echo_defs::prompt::PromptStore for PromptService {
    fn replace(&self, owner: &str, blocks: Vec<echo_defs::prompt::PromptBlock>) {
        self.contributed
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(owner.to_string(), blocks);
    }
    fn remove(&self, owner: &str) {
        self.contributed
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(owner);
    }
    fn invalidate(&self) {
        if let Ok(mut cache) = self.system_prompt_cache.try_write() {
            *cache = None;
        }
    }
}
