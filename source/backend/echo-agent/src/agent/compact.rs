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
    /// Some = 用户指定的单个会话；None = 内部全量维护。
    session_id: Option<&'a str>,
}

impl CompactionHost for AgentCompactionHost<'_> {
    fn preview(&self, keep_recent: usize) -> Result<CompactPreview, String> {
        let mut preview = self.agent.trunk.compact_preview(keep_recent)?;
        if let Some(session_id) = self.session_id {
            preview
                .groups
                .retain(|group| group.session.as_deref() == Some(session_id));
            preview.total_replaced = preview
                .groups
                .iter()
                .map(|group| group.replaced_count)
                .sum();
            if preview.groups.is_empty() {
                return Err(format!("会话 {session_id} 历史不足，无需压缩"));
            }
        }
        Ok(preview)
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

    /// 摘要备用出口：**全局默认 profile（`active_api`）**（2026-10 修复）。
    ///
    /// 压缩此前只认人格级 provider（`[agent.teams.*] api_profile`）。人格
    /// profile 欠费（HTTP 402）时，聊天还能靠会话级覆盖正常工作，压缩却整份
    /// 降级成统计文案——历史信息全丢。这里按全局默认 profile 现建一个出口，
    /// 供 `summarize_preview` 在主出口失败时重试一次。
    ///
    /// 与当前出口等价（key / model / base_url 三项全同）时返回 `None`：
    /// 重试同一个出口没有意义，也避免白花一次调用。
    fn fallback_provider(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<Arc<dyn crate::llm::LlmProvider>>> + Send + '_>,
    > {
        Box::pin(async move {
            let current = self.agent.model.config.read().await.clone();
            let mut global = current.clone();
            global.apply_active_profile();
            let same = global.effective_api_key() == current.effective_api_key()
                && global.model == current.model
                && global.effective_base_url() == current.effective_base_url();
            if same {
                return None;
            }
            global.api_key = global.effective_api_key();
            global.base_url = global.effective_base_url();
            let boxed = match crate::llm::create_provider(&global) {
                Ok(provider) => provider,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "compaction: fallback provider build failed; 主出口失败时将降级为统计文案"
                    );
                    return None;
                }
            };
            let mut provider: Arc<dyn crate::llm::LlmProvider> = Arc::from(boxed);
            // 备用出口同样过安全卡口与计量——否则「换了出口就漏脱敏/用量」。
            let metrics = self
                .agent
                .instrumentation
                .metrics
                .read()
                .ok()
                .and_then(|guard| guard.as_ref().cloned());
            if let Some(metrics) = metrics {
                provider = crate::llm::wrap_metering(provider, metrics, global.active_api.clone());
            }
            let redactor = self
                .agent
                .instrumentation
                .redactor
                .read()
                .ok()
                .and_then(|guard| guard.as_ref().cloned());
            if let Some(redactor) = redactor {
                provider = crate::llm::wrap_redacting(provider, redactor);
            }
            tracing::info!(
                profile = %global.active_api,
                model = %global.model,
                "compaction: fallback provider ready"
            );
            Some(provider)
        })
    }

    /// 进度 → 线协议事件（面板画进度条）。
    ///
    /// 此前是把一句话塞进 `BackendEvent::Error`：面板会把它当**错误**处理
    /// （弹提示、冻结流式行），而且没有任何分子分母，画不出进度。现在走
    /// 专用事件，`Error` 只留给真正的失败与终态结果。
    fn progress(&self, progress: &echo_session::CompactionProgress) {
        self.agent
            .emit(crate::event::BackendEvent::CompactionProgress {
                team_id: self.agent.team_id(),
                session_id: self.session_id.map(str::to_string),
                phase: progress.phase.as_str().to_string(),
                done: progress.done,
                total: progress.total,
                sessions: progress.sessions,
                message: progress.message.clone(),
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
    /// 归档**单个会话**并上报进度（`archive` → `done` / `failed`）。
    pub async fn archive_session_with_progress(&self, session_id: &str) -> Result<String, String> {
        use echo_session::CompactionPhase;
        let emit = |phase: CompactionPhase, done, total, sessions, message: String| {
            self.emit(crate::event::BackendEvent::CompactionProgress {
                team_id: self.team_id(),
                session_id: Some(session_id.to_string()),
                phase: phase.as_str().to_string(),
                done,
                total,
                sessions,
                message,
            });
        };
        emit(
            CompactionPhase::Archive,
            0,
            0,
            1,
            format!("正在归档会话 {session_id} 的快照…"),
        );
        let result = self.trunk.archive_session(session_id).await;
        match &result {
            Ok(path) => emit(
                CompactionPhase::Done,
                1,
                1,
                1,
                format!("历史已归档：{path}"),
            ),
            Err(error) => emit(
                CompactionPhase::Failed,
                0,
                0,
                1,
                format!("归档失败：{error}"),
            ),
        }
        result
    }

    /// 内部全量维护入口；用户命令必须走 compact_session_history。
    pub async fn compact_history(&self, keep_recent: usize) -> Result<String, String> {
        self.compact_history_scoped(None, keep_recent).await
    }

    /// 只压缩指定会话；其他会话不参与预览、摘要与落地。
    pub async fn compact_session_history(
        &self,
        session_id: &str,
        keep_recent: usize,
    ) -> Result<String, String> {
        self.compact_history_scoped(Some(session_id), keep_recent)
            .await
    }

    async fn compact_history_scoped(
        &self,
        session_id: Option<&str>,
        keep_recent: usize,
    ) -> Result<String, String> {
        let _compact_guard = self.trunk.compact_lock().await;
        let host = AgentCompactionHost {
            agent: self,
            session_id,
        };
        let result = echo_plugin_loop::compaction::compact_history(&host, keep_recent).await;
        // 失败也上报一次终态：否则面板的进度条会**永远转下去**（它只能靠
        // 这个事件判断结束）。注意成功路径已在策略内部发过 `done`，这里
        // 只补失败；`Error` 事件仍由调用方发（那是给用户的提示，不是进度）。
        if let Err(reason) = &result {
            use echo_plugin_loop::compaction::CompactionHost as _;
            host.progress(&echo_session::CompactionProgress::step(
                echo_session::CompactionPhase::Failed,
                format!("压缩失败：{reason}"),
            ));
        }
        result
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
        let host = AgentCompactionHost {
            agent: &agent,
            session_id: None,
        };
        let from_agent = agent.model.provider.read().await.clone();
        let from_port = host.provider().await;
        assert!(
            Arc::ptr_eq(&from_agent, &from_port),
            "端口必须交出同一份 provider（否则计量/脱敏装饰会丢）"
        );
    }

    /// 一个窗口的压缩只能改写自己的目标会话，不能波及同人格的其他窗口。
    #[tokio::test]
    async fn compacting_one_session_preserves_every_other_session() {
        let agent = Agent::new(
            Arc::new(MockProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                reply: "【当前目标】继续\n【已完成】旧对话\n【进行中】无\n【关键信息】测试".into(),
            }),
            crate::config::AgentConfig::default(),
            crate::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
        );
        let a = "local:workspace:proj/win-a:local_user";
        let b = "local:workspace:proj/win-b:local_user";
        for session in [a, b] {
            let events = (0..6)
                .map(|i| {
                    echo_session::SessionEvent::UserMessage(echo_session::event::UserMessage {
                        session: Some(session.into()),
                        content: format!("message-{i}"),
                        timestamp: i,
                        message_sequence: None,
                        source: None,
                        images: vec![],
                    })
                })
                .collect();
            agent.trunk.import_session_events(session, events);
        }

        agent
            .compact_session_history(a, 2)
            .await
            .expect("compact A");
        let events = agent.trunk.event_log();
        let compacted: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                echo_session::SessionEvent::Compaction(compaction) => compaction.session.as_deref(),
                _ => None,
            })
            .collect();
        assert_eq!(compacted, vec![a]);
        let b_events: Vec<_> = events
            .iter()
            .filter(|event| event.session().as_deref() == Some(b))
            .collect();
        assert_eq!(b_events.len(), 6, "B 的原始事件必须全部保留");
        assert!(!b_events
            .iter()
            .any(|event| matches!(event, echo_session::SessionEvent::Compaction(_))));
    }
}
