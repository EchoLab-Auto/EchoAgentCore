//! 上下文压缩：**策略在插件里，宿主只提供五件事**（全插件化 S3）。
//!
//! ## 搬家后的分工
//!
//! | 策略（`echo_plugin_loop::compaction`） | 宿主（本文件） |
//! |---|---|
//! | 转录渲染（角色标签 / 分层裁剪 / 二分找截断点） | `compact_preview`（日志自己算） |
//! | 摘要提示词 + 单组调用（超时 / 空输出 / 错误） | provider 从哪来 |
//! | 失败回退的规则文案 | 归档快照（写 `archives/`） |
//! | 结果文案（几条 / 几组 / 回退原因） | 落地（追加 `CompactionEvent`）+ 进度事件 |
//!
//! 于是 732 行的 `compact.rs` 只剩这里的**端口实现**与一行委托。搬走的
//! ~290 行是纯策略——它此前住在宿主只因为收尾那几步要 `TrunkStore`。
//!
//! ## 为什么互斥锁留在宿主
//!
//! `TrunkStore::compact_lock` 的守卫是宿主的类型。把它塞进端口意味着插件
//! 要认识 `TrunkStore` 的锁类型——那比"宿主先拿锁、再调策略"多绕一层。
//! 守卫活在宿主的栈帧上，插件 future 被 await 的整段时间它都有效。

use std::sync::Arc;

use echo_plugin_loop::compaction::CompactionHost;
use echo_session::{CompactPreview, CompactionApplied, CompactionSummary};

use crate::agent::Agent;

/// 宿主侧端口实现：借用 `&Agent`（trunk / provider / 事件出口都在它身上）。
struct AgentCompactionHost<'a> {
    agent: &'a Agent,
}

impl CompactionHost for AgentCompactionHost<'_> {
    fn preview(&self, keep_recent: usize) -> Result<CompactPreview, String> {
        self.agent.trunk.compact_preview(keep_recent)
    }

    fn archive_snapshot(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
    {
        Box::pin(async move { self.agent.trunk.archive_snapshot("precompact").await })
    }

    fn provider(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Arc<dyn crate::llm::LlmProvider>> + Send + '_>,
    > {
        Box::pin(async move { self.agent.model.provider.read().await.clone() })
    }

    fn progress(&self, message: String) {
        self.agent.emit(crate::event::BackendEvent::Error {
            session_id: None,
            message,
        });
    }

    fn apply<'a>(
        &'a self,
        keep_recent: usize,
        summaries: Vec<CompactionSummary>,
        archive: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<CompactionApplied, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.agent
                .trunk
                .apply_compaction(keep_recent, &summaries, archive)
                .await
        })
    }
}

impl Agent {
    /// 压缩历史：**预览 → 归档快照 → 逐组摘要 → 落地**。
    ///
    /// 算法在插件里（`echo_plugin_loop::compaction::compact_history`）；本方法
    /// 只负责拿**压缩互斥锁**（2026-10 巡检 🔴：double compact / compact 与
    /// clear_history 竞争会按过时摘要重写日志、统计错乱）然后委托。
    pub async fn compact_history(&self, keep_recent: usize) -> Result<String, String> {
        let _compact_guard = self.trunk.compact_lock().await;
        let host = AgentCompactionHost { agent: self };
        echo_plugin_loop::compaction::compact_history(&host, keep_recent).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::MockProvider;

    /// 历史不足时压缩**明确报错**（不是静默成功），且错误文案说清原因。
    #[tokio::test]
    async fn compacting_too_little_history_reports_why() {
        let agent = Agent::new(
            Arc::new(MockProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                reply: String::new(),
            }),
            crate::config::AgentConfig::default(),
            crate::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
        );
        let error = agent
            .compact_history(10)
            .await
            .expect_err("空历史不该压缩成功");
        assert!(error.contains("历史不足"), "{error}");
    }

    /// 端口把 `provider()` 解析成**同一份** provider（不是新造的）。
    #[tokio::test]
    async fn provider_port_shares_the_agent_provider() {
        let agent = Agent::new(
            Arc::new(MockProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                reply: String::new(),
            }),
            crate::config::AgentConfig::default(),
            crate::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
        );
        let host = AgentCompactionHost { agent: &agent };
        let from_agent = agent.model.provider.read().await.clone();
        let from_port = host.provider().await;
        assert!(
            Arc::ptr_eq(&from_agent, &from_port),
            "端口必须交出同一份 provider（否则计量/脱敏装饰会丢）"
        );
    }
}
