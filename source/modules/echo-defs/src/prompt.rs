//! 通用提示词存储与模块主动贡献接口。宿主不认识具体业务模块。

use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptBlock {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub content: String,
}

/// 模块持此句柄主动插入、更新或删除自己的提示词块。
pub trait PromptStore: Send + Sync {
    fn replace(&self, owner: &str, blocks: Vec<PromptBlock>);
    fn remove(&self, owner: &str);
    fn invalidate(&self);
}

/// Agent 每轮组装前调用；模块在此同步计算并写入本轮提示词块。
/// 返回即表示本轮贡献已完成，不引入异步竞态。
pub trait PromptRefresher: Send + Sync {
    fn refresh(&self, context: &serde_json::Value);
}

pub type SharedPromptStore = Arc<dyn PromptStore>;
pub type SharedPromptRefresher = Arc<dyn PromptRefresher>;


/// 通用名单型能力控制（技能/规则包等均可实现），由模块注册给宿主。
#[async_trait::async_trait]
pub trait NamedCapabilityController: Send + Sync {
    async fn apply(&self, enabled: Vec<String>, disabled: Vec<String>, global_disabled: Vec<String>);
    async fn reload(&self) -> Result<bool, String> { Ok(false) }
    async fn set_enabled(&self, _name: &str, _enabled: bool) -> bool { false }
    fn set_package_enabled(&self, _package: &str, _enabled: bool) {}
    async fn emit_list(&self) {}
    fn packages(&self) -> Vec<String> { Vec::new() }
}
