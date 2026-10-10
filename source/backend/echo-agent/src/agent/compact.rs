//! LLM-generated compaction summaries, with a rule-based fallback.
//!
//! Compaction replaces a prefix of the session event log with one summary
//! ([`echo_session::CompactionEvent`]). This module turns the replaced events
//! into a *hand-off summary*: the caller previews a compaction
//! ([`TrunkStore::compact_preview`]), archives the raw events, and asks the
//! model here to summarize each session group. On any failure (no provider,
//! timeout, error, empty output) the group degrades to a rule summary so a
//! compaction never fails for lack of an LLM.
//!
//! [`TrunkStore::compact_preview`]: crate::session::TrunkStore::compact_preview

use std::sync::Arc;

use echo_session::SessionEvent;

use crate::llm::{estimate_tokens, ChatMessage, ChatRequest, LlmProvider};
use crate::session::{CompactGroup, CompactionSummary};

/// Transcript token budget for one summarization call. The replaced prefix is
/// usually far smaller; when it is not, [`TRANSCRIPT_TIERS`] shrink it.
pub(crate) const TRANSCRIPT_TOKEN_BUDGET: usize = 50_000;

/// Output cap for one summary (the prompt asks for ≤ 800 characters).
pub(crate) const SUMMARY_MAX_TOKENS: u32 = 1500;

/// Deadline for one group's summarization call.
pub(crate) const SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Per-event character caps for one transcript tier.
#[derive(Debug, Clone, Copy)]
struct TranscriptCaps {
    user: usize,
    assistant: usize,
    tool_result: usize,
}

/// Progressively tighter caps: try the first, fall back until it fits.
const TRANSCRIPT_TIERS: [TranscriptCaps; 3] = [
    TranscriptCaps {
        user: 1200,
        assistant: 2000,
        tool_result: 1500,
    },
    TranscriptCaps {
        user: 600,
        assistant: 800,
        tool_result: 500,
    },
    TranscriptCaps {
        user: 300,
        assistant: 300,
        tool_result: 200,
    },
];

const SUMMARY_SYSTEM_PROMPT: &str = "\
你是一个对话历史压缩器。输入是一段将被压缩掉的旧对话记录（时间正序），\
它即将从上下文里移除。请输出一份**交接摘要**，让一个失去原始记录的 agent \
能靠它继续工作。

要求：
- 固定四节：【当前目标】【已完成】【进行中】【关键信息】
- 【关键信息】保留：文件路径、函数/命令名、ID、数量、结论、决定、约定、
  用户偏好与硬性要求
- 具体优先：写「改 ContextView 的 blockPct 口径」而不是「修了一个显示问题」
- 不写套话、不复述过程流水账；只写对后续有用的
- 语言与记录一致（中文记录用中文，英文记录用英文）
- 全文不超过 800 字
- 直接输出摘要正文（含四节小标题），不要任何前后缀或额外解释";

/// Summarize every group in a compaction preview (in order, one call each).
///
/// Never fails as a whole: a group whose LLM call fails gets the rule
/// fallback. Returns `(summaries, llm_count, fallback_count)`.
pub(crate) async fn summarize_preview(
    provider: &Arc<dyn LlmProvider>,
    preview: &crate::session::CompactPreview,
    keep_recent: usize,
) -> (Vec<SummaryOrFallback>, usize, usize) {
    let mut out = Vec::new();
    let mut llm_count = 0usize;
    let mut fallback_count = 0usize;
    for group in &preview.groups {
        let (summary, from_llm, fallback_reason) = match summarize_group(provider, group).await {
            Ok(body) => {
                llm_count += 1;
                (body, true, None)
            }
            Err(reason) => {
                fallback_count += 1;
                (
                    rule_fallback_summary(group, keep_recent, &reason),
                    false,
                    Some(reason),
                )
            }
        };
        tracing::info!(
            session = group.session.as_deref().unwrap_or("<legacy>"),
            replaced = group.replaced_count,
            from_llm,
            "compaction: group summarized"
        );
        out.push(SummaryOrFallback {
            session: group.session.clone(),
            summary,
            from_llm,
            fallback_reason,
        });
    }
    (out, llm_count, fallback_count)
}

/// A prepared summary plus its provenance (LLM vs rule fallback).
#[derive(Debug, Clone)]
pub(crate) struct SummaryOrFallback {
    pub session: Option<String>,
    pub summary: String,
    pub from_llm: bool,
    pub fallback_reason: Option<String>,
}

/// Summarize one group with the model. Err carries a short reason suitable
/// for the fallback text (and the operator-facing report).
pub(crate) async fn summarize_group(
    provider: &Arc<dyn LlmProvider>,
    group: &CompactGroup,
) -> Result<String, String> {
    if group.replaced.is_empty() {
        return Err("组内没有可压缩事件".into());
    }
    let transcript = render_transcript(&group.replaced);
    let request = ChatRequest {
        model: provider.default_model().to_string(),
        messages: vec![
            ChatMessage::system(SUMMARY_SYSTEM_PROMPT),
            ChatMessage::user(format!("旧对话记录：\n\n{transcript}")),
        ],
        tools: None,
        temperature: None,
        max_tokens: Some(SUMMARY_MAX_TOKENS),
    };
    let response = match tokio::time::timeout(SUMMARY_TIMEOUT, provider.chat(&request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(format!("摘要请求失败：{error}")),
        Err(_) => return Err("摘要请求超时".into()),
    };
    let body = response.content.unwrap_or_default();
    let body = body.trim();
    if body.is_empty() {
        return Err("摘要输出为空".into());
    }
    Ok(body.to_string())
}

/// Rule summary used when the model is unavailable. Keeps the historic
/// statistics shape so old and new compactions read alike.
pub(crate) fn rule_fallback_summary(
    group: &CompactGroup,
    keep_recent: usize,
    reason: &str,
) -> String {
    format!(
        "已压缩 {} 条历史事件（{} 条用户消息，{} 次工具调用），保留最近 {} 条。\
         自动摘要不可用（{}），本组仅保留统计信息。",
        group.replaced_count, group.users, group.tools, keep_recent, reason
    )
}

/// Render the replaced events as a role-tagged transcript, shrinking detail
/// until it fits [`TRANSCRIPT_TOKEN_BUDGET`]:
/// tighter caps first, then dropping oldest events (newest context wins).
pub(crate) fn render_transcript(events: &[SessionEvent]) -> String {
    for caps in TRANSCRIPT_TIERS {
        let text = render_with_caps(events, caps);
        if estimate_tokens(&text) <= TRANSCRIPT_TOKEN_BUDGET {
            return text;
        }
    }
    // Last resort: keep the newest events that fit. Dropping leading events
    // shrinks the transcript monotonically, so binary-search the cutoff
    // instead of re-rendering on every drop (O(n log n) vs O(n²)).
    let caps = TRANSCRIPT_TIERS[TRANSCRIPT_TIERS.len() - 1];
    let fits = |start: usize| {
        estimate_tokens(&render_with_caps(&events[start..], caps)) <= TRANSCRIPT_TOKEN_BUDGET
    };
    if fits(0) {
        return render_with_caps(events, caps);
    }
    let (mut lo, mut hi) = (0usize, events.len() - 1);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if fits(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let mut text = render_with_caps(&events[lo..], caps);
    // Pathological single event larger than the whole budget: hard-truncate.
    if estimate_tokens(&text) > TRANSCRIPT_TOKEN_BUDGET {
        let head: String = text.chars().take(TRANSCRIPT_TOKEN_BUDGET / 2).collect();
        text = format!("{head}…（截断）");
    }
    if lo > 0 {
        format!("（更早记录已省略）\n{text}")
    } else {
        text
    }
}

fn render_with_caps(events: &[SessionEvent], caps: TranscriptCaps) -> String {
    let mut lines: Vec<String> = Vec::new();
    // Tool call ids already spelled out by an assistant message.
    let mut rendered_calls: std::collections::HashSet<String> = Default::default();
    for event in events {
        match event {
            SessionEvent::UserMessage(m) => {
                lines.push(format!("[用户] {}", truncate_chars(&m.content, caps.user)));
            }
            SessionEvent::AssistantMessage(m) => {
                let mut line = String::new();
                if !m.content.trim().is_empty() {
                    line.push_str(&truncate_chars(&m.content, caps.assistant));
                }
                if !m.tool_calls.is_empty() {
                    let names: Vec<String> = m
                        .tool_calls
                        .iter()
                        .map(|call| {
                            rendered_calls.insert(call.id.clone());
                            call.name.clone()
                        })
                        .collect();
                    if !line.is_empty() {
                        line.push(' ');
                    }
                    line.push_str(&format!("（调用工具：{}）", names.join("、")));
                }
                if !line.is_empty() {
                    lines.push(format!("[助手] {line}"));
                }
            }
            SessionEvent::ToolCall(call) => {
                // Covered by its assistant message (the log's common shape);
                // only standalone calls get their own line.
                if !rendered_calls.contains(&call.id) {
                    lines.push(format!("[工具调用] {}", call.name));
                }
            }
            SessionEvent::ToolResult(result) => {
                lines.push(format!(
                    "[工具结果] {}",
                    truncate_chars(&result.result, caps.tool_result)
                ));
            }
            SessionEvent::Compaction(c) => {
                lines.push(format!(
                    "[更早摘要] {}",
                    truncate_chars(&c.summary, caps.assistant)
                ));
            }
        }
    }
    lines.join("\n")
}

/// Char-boundary-safe truncation with an explicit ellipsis marker.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}…（截断）")
}

impl crate::agent::Agent {
    /// 压缩历史（2026-09 三件套）：**预览 → 归档快照 → 逐组摘要 → 落地**。
    ///
    /// 摘要优先由 LLM 生成（交接摘要，失败按组回退规则文案）；压缩前把
    /// 当前会话整体快照归档到 `archives/`（失败不阻塞，仅在结果注明）。
    /// 返回面向用户的一行结果。
    pub async fn compact_history(&self, keep_recent: usize) -> Result<String, String> {
        // 压缩互斥（2026-10 巡检 🔴）：double compact / compact 与
        // clear_history 竞争会按过时摘要重写日志、统计错乱。
        let _compact_guard = self.trunk.compact_lock().await;
        let preview = self.trunk.compact_preview(keep_recent)?;
        let provider = self.provider.read().await.clone();
        let archive = match self.trunk.archive_snapshot("precompact").await {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!(%error, "compaction: archive snapshot failed; continuing");
                None
            }
        };
        self.emit(crate::event::BackendEvent::Error {
            session_id: None,
            message: format!(
                "正在压缩历史：{} 组、{} 条事件，生成 LLM 摘要中…",
                preview.groups.len(),
                preview.total_replaced
            ),
        });
        let (summaries, llm_count, fallback_count) =
            summarize_preview(&provider, &preview, keep_recent).await;
        let applied = self
            .trunk
            .apply_compaction(
                keep_recent,
                &summaries
                    .iter()
                    .map(|s| CompactionSummary {
                        session: s.session.clone(),
                        summary: s.summary.clone(),
                    })
                    .collect::<Vec<_>>(),
                archive.as_deref(),
            )
            .await?;
        let mut message = format!(
            "已压缩 {} 条历史事件（{} 组：LLM 摘要 {} 组 / 规则回退 {} 组），保留最近 {} 条",
            applied.total_replaced, applied.groups, llm_count, fallback_count, applied.kept
        );
        if fallback_count > 0 {
            let mut reasons: Vec<String> = summaries
                .iter()
                .filter(|s| !s.from_llm)
                .filter_map(|s| s.fallback_reason.clone())
                .collect();
            reasons.dedup();
            message.push_str(&format!("；回退原因：{}", reasons.join("；")));
        }
        match &archive {
            Some(path) => message.push_str(&format!("；原事件已归档：{path}")),
            None => message.push_str("；注意：归档快照失败或未启用持久化，原事件未备份"),
        }
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_session::event::{AssistantMessage, ToolCallEvent, ToolResultEvent, UserMessage};
    use std::sync::Arc;

    fn user(content: &str) -> SessionEvent {
        SessionEvent::UserMessage(UserMessage {
            content: content.into(),
            timestamp: 0,
            message_sequence: None,
            source: None,
            images: vec![],
            session: Some("s".into()),
        })
    }

    fn assistant(content: &str) -> SessionEvent {
        SessionEvent::AssistantMessage(AssistantMessage {
            content: content.into(),
            reasoning_content: Some("内部推理不应出现在转录里".into()),
            tool_calls: vec![],
            session: Some("s".into()),
        })
    }

    #[test]
    fn transcript_tags_roles_and_skips_reasoning() {
        let events = vec![
            user("先改 A 再改 B"),
            SessionEvent::ToolCall(ToolCallEvent {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
                session: Some("s".into()),
                started_at_ms: None,
            }),
            SessionEvent::ToolResult(ToolResultEvent {
                tool_call_id: "c1".into(),
                result: "done".into(),
                images: vec![],
                session: Some("s".into()),
                elapsed_ms: None,
            }),
            assistant("两处都改完了"),
        ];
        let text = render_transcript(&events);
        assert!(text.contains("[用户] 先改 A 再改 B"));
        assert!(text.contains("[工具调用] bash"));
        assert!(text.contains("[工具结果] done"));
        assert!(text.contains("[助手] 两处都改完了"));
        assert!(
            !text.contains("内部推理"),
            "reasoning is internal, never in the transcript"
        );
    }

    #[test]
    fn assistant_tool_calls_absorb_standalone_tool_call_events() {
        // The common log shape: the assistant message carries the calls and
        // separate ToolCall events echo them. They must not double-render.
        let events = vec![
            SessionEvent::AssistantMessage(AssistantMessage {
                content: "跑一下".into(),
                reasoning_content: None,
                tool_calls: vec![echo_defs::message::ToolCall {
                    id: "c9".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
                session: Some("s".into()),
            }),
            SessionEvent::ToolCall(ToolCallEvent {
                id: "c9".into(),
                name: "bash".into(),
                arguments: "{}".into(),
                session: Some("s".into()),
                started_at_ms: None,
            }),
        ];
        let text = render_transcript(&events);
        assert!(text.contains("（调用工具：bash）"));
        assert_eq!(text.matches("bash").count(), 1, "no duplicate line: {text}");
    }

    #[test]
    fn transcript_shrinks_to_fit_the_budget() {
        // A single huge tool result: tier caps bound it far below the budget,
        // so the transcript must be tiny, not 200k tokens.
        let huge = "x".repeat(400_000);
        let events = vec![SessionEvent::ToolResult(ToolResultEvent {
            tool_call_id: "c1".into(),
            result: huge,
            images: vec![],
            session: Some("s".into()),
            elapsed_ms: None,
        })];
        let text = render_transcript(&events);
        assert!(estimate_tokens(&text) <= TRANSCRIPT_TOKEN_BUDGET);
        assert!(text.contains("截断"));
    }

    #[test]
    fn transcript_drops_oldest_events_when_caps_are_not_enough() {
        // 20k messages of 300 chars each: even tier-3 caps overflow, so the
        // oldest events must be dropped and the newest kept.
        let events: Vec<SessionEvent> = (0..20_000)
            .map(|i| user(&format!("消息{i}：{}", "内".repeat(300))))
            .collect();
        let text = render_transcript(&events);
        assert!(estimate_tokens(&text) <= TRANSCRIPT_TOKEN_BUDGET);
        assert!(text.starts_with("（更早记录已省略）"), "oldest dropped");
        assert!(text.contains("消息19999"));
    }

    #[test]
    fn truncate_chars_is_multibyte_safe() {
        assert_eq!(truncate_chars("短", 5), "短");
        let out = truncate_chars("一二三四五", 2);
        assert!(out.starts_with("一二"));
        assert!(out.ends_with("（截断）"));
    }

    fn group(events: Vec<SessionEvent>) -> CompactGroup {
        CompactGroup {
            session: Some("s".into()),
            replaced_count: events.len(),
            users: events
                .iter()
                .filter(|e| matches!(e, SessionEvent::UserMessage(_)))
                .count(),
            tools: events
                .iter()
                .filter(|e| matches!(e, SessionEvent::ToolCall(_)))
                .count(),
            replaced: events,
        }
    }

    struct StubProvider {
        reply: Result<String, String>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }
        fn default_model(&self) -> &str {
            "stub-model"
        }
        async fn chat(
            &self,
            _request: &ChatRequest,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            match &self.reply {
                Ok(text) => Ok(crate::llm::ChatResponse {
                    content: Some(text.clone()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    usage: Default::default(),
                    stop_reason: Some("end_turn".into()),
                }),
                Err(message) => Err(crate::llm::LlmError::Config(message.clone())),
            }
        }
        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<crate::llm::ChatChunk>,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Config("stream unsupported".into()))
        }
    }

    #[tokio::test]
    async fn summarize_group_returns_model_body() {
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider {
            reply: Ok("【当前目标】x\n【已完成】y".into()),
        });
        let body = summarize_group(&provider, &group(vec![user("你好")]))
            .await
            .expect("summarize");
        assert!(body.contains("【当前目标】"));
    }

    #[tokio::test]
    async fn summarize_group_rejects_whitespace_output() {
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider {
            reply: Ok("   \n  ".into()),
        });
        let err = summarize_group(&provider, &group(vec![user("你好")]))
            .await
            .unwrap_err();
        assert!(err.contains("为空"), "got: {err}");
    }

    #[tokio::test]
    async fn summarize_group_reports_provider_errors() {
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider {
            reply: Err("no api key".into()),
        });
        let err = summarize_group(&provider, &group(vec![user("你好")]))
            .await
            .unwrap_err();
        assert!(err.contains("摘要请求失败"), "got: {err}");
    }

    #[tokio::test]
    async fn summarize_preview_falls_back_per_group() {
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider {
            reply: Err("no api key".into()),
        });
        let preview = crate::session::CompactPreview {
            groups: vec![
                group(vec![user("a"), user("b")]),
                CompactGroup {
                    session: None,
                    replaced_count: 1,
                    users: 1,
                    tools: 0,
                    replaced: vec![user("legacy")],
                },
            ],
            total_replaced: 3,
            total_events: 9,
        };
        let (summaries, llm, fallback) = summarize_preview(&provider, &preview, 40).await;
        assert_eq!(llm, 0);
        assert_eq!(fallback, 2);
        assert_eq!(summaries.len(), 2);
        assert!(summaries[0].summary.contains("自动摘要不可用"));
        assert!(summaries[1].summary.contains("自动摘要不可用"));
        assert_eq!(summaries[1].session, None);
        // The fallback keeps the historic statistics shape.
        assert!(summaries[0].summary.contains("2 条用户消息"));
    }

    #[test]
    fn rule_fallback_mentions_counts_and_reason() {
        let g = group(vec![user("a"), user("b")]);
        let text = rule_fallback_summary(&g, 40, "摘要请求超时");
        assert!(text.contains("2 条历史事件"));
        assert!(text.contains("2 条用户消息"));
        assert!(text.contains("保留最近 40 条"));
        assert!(text.contains("摘要请求超时"));
        // Body only — the projection adds the marker.
        assert!(!text.contains("[历史摘要]"));
    }

    // ── Agent 级端到端：预览 → 归档 → LLM 摘要 → 落地 ──

    fn test_agent(provider: Arc<dyn LlmProvider>) -> crate::agent::Agent {
        crate::agent::Agent::new(
            provider,
            crate::config::AgentConfig::default(),
            crate::skill::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_adapter::AdapterRegistry::new()),
        )
    }

    fn seed_history(agent: &crate::agent::Agent, count: usize) {
        for i in 0..count {
            agent
                .trunk
                .append_event(SessionEvent::UserMessage(UserMessage {
                    content: format!("第 {i} 条消息"),
                    timestamp: 0,
                    message_sequence: None,
                    source: None,
                    images: vec![],
                    session: Some(crate::session::SessionKey::local_tui().to_session_id()),
                }));
        }
    }

    #[tokio::test]
    async fn agent_compaction_end_to_end_with_llm_summary() {
        let dir = std::env::temp_dir().join(format!("echo-compact-e2e-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sessions.json");
        let _ = std::fs::remove_file(&path);

        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider {
            reply: Ok("【当前目标】完成压缩改造\n【关键信息】compact.rs".into()),
        });
        let agent = test_agent(provider);
        agent.set_session_persist_path(&path);
        seed_history(&agent, 6);
        agent.trunk.save_now().await;

        let message = agent.compact_history(2).await.expect("compact");
        assert!(message.contains("已压缩 4 条历史事件"), "got: {message}");
        assert!(message.contains("LLM 摘要 1 组"), "got: {message}");
        assert!(message.contains("规则回退 0 组"), "got: {message}");
        assert!(message.contains("precompact"), "archive path: {message}");

        // 归档文件确实落地，且含将被压缩的原始事件。
        let archive_path = message
            .split("已归档：")
            .nth(1)
            .expect("archive path in message")
            .trim();
        let archived = std::fs::read_to_string(archive_path).expect("archive readable");
        assert!(archived.contains("第 3 条消息"), "archive keeps raw events");

        // 事件日志：摘要正文（无前缀）+ 归档字段 + 保留尾部。
        let events = agent.trunk.event_log();
        match &events[0] {
            SessionEvent::Compaction(c) => {
                assert!(c.summary.starts_with("【当前目标】"));
                assert!(!c.summary.contains("[历史摘要]"), "body is marker-free");
                assert_eq!(c.archive.as_deref(), Some(archive_path));
                assert_eq!(c.replaced_count, 4);
            }
            other => panic!("expected compaction first, got {other:?}"),
        }

        // 模型可见投影：恰好一个前缀。
        let sid = crate::session::SessionKey::local_tui().to_session_id();
        let history = agent.trunk.snapshot_for(&sid).await;
        assert_eq!(history.len(), 3, "summary + 2 kept");
        assert!(
            history[0].content.starts_with("[历史摘要] 【当前目标】"),
            "got: {}",
            history[0].content
        );
        assert!(!history[0].content.contains("[历史摘要] [历史摘要]"));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(archive_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[tokio::test]
    async fn agent_compaction_falls_back_when_provider_fails() {
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider {
            reply: Err("no api key configured".into()),
        });
        let agent = test_agent(provider);
        seed_history(&agent, 6);

        let message = agent.compact_history(2).await.expect("compact");
        assert!(message.contains("规则回退 1 组"), "got: {message}");
        assert!(message.contains("回退原因"), "got: {message}");

        let events = agent.trunk.event_log();
        match &events[0] {
            SessionEvent::Compaction(c) => {
                assert!(
                    c.summary.contains("自动摘要不可用"),
                    "fallback keeps counts: {}",
                    c.summary
                );
                // 未配置持久化 → 无归档路径，如实置空。
                assert_eq!(c.archive, None);
            }
            other => panic!("expected compaction first, got {other:?}"),
        }
        // 无持久化时结果里注明未备份。
        assert!(message.contains("未备份"), "got: {message}");
    }

    #[tokio::test]
    async fn agent_compaction_rejects_short_history() {
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider {
            reply: Ok("n/a".into()),
        });
        let agent = test_agent(provider);
        seed_history(&agent, 3);
        let err = agent.compact_history(40).await.unwrap_err();
        assert!(err.contains("历史不足"), "got: {err}");
    }
}
