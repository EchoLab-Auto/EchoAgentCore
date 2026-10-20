//! 上下文压缩**策略**：把被替换的事件变成一份交接摘要。
//!
//! ## 为什么压缩策略归 loop 插件
//!
//! 压缩是"循环怎么管理自己的上下文"的一部分：它决定哪些事件会被摘掉、拿
//! 什么替换它们、替换物长什么样。此前这段住在宿主 `agent/compact.rs`（732
//! 行，其中 ~290 行是纯策略），只因为收尾那几步（拿锁、归档、落地）需要
//! `TrunkStore`。
//!
//! 拆开后的分工：
//!
//! | 策略（本模块） | 宿主（[`CompactionHost`]） |
//! |---|---|
//! | 转录渲染（角色标签 + 分层裁剪 + 二分找截断点） | 压缩预览（日志自己算） |
//! | 摘要提示词与单组调用（超时 / 空输出 / 错误） | provider 从哪来 |
//! | 失败回退的规则文案 | 归档快照（写 `archives/`） |
//! | 结果文案（几条 / 几组 / 回退原因） | 落地（追加 `CompactionEvent`）+ 进度事件 |
//!
//! ## 两条不变量（搬过来时逐条保留）
//!
//! 1. **压缩永不因缺 LLM 而失败**：任一组出错（超时 / 报错 / 空输出 / 无
//!    provider）都退化成规则文案，其余组照常。
//! 2. **转录按预算分层收缩**：先收紧每事件字符上限（三档），仍超预算就丢最老
//!    的事件（新上下文优先）——用二分找截断点，不是逐条重渲染（O(n log n)）。
//!
//! 另外两条渲染口径：**推理与模型往返观测不进转录**（它们对"这段对话讲了
//! 什么"没有信息量，却占满预算）；**工具调用若已被 assistant 消息列举就
//! 不重复成行**（日志的常见形状是"消息带 tool_calls + 独立 ToolCall ���件"）。

use std::sync::Arc;

use echo_defs::llm::LlmProvider;
use echo_defs::message::{ChatMessage, ChatRequest};
use echo_defs::token::estimate_tokens;
use echo_session::event::SessionEvent;
use echo_session::{CompactGroup, CompactPreview, CompactionSummary};

/// 一次摘要调用的转录 token 预算（被替换的前缀通常远小于它；超了就按
/// [`TRANSCRIPT_TIERS`] 收缩）。
pub const TRANSCRIPT_TOKEN_BUDGET: usize = 50_000;

/// 一次摘要的输出上限（提示词要求 ≤ 800 字）。
pub const SUMMARY_MAX_TOKENS: u32 = 1500;

/// 单组摘要的截止时间。
pub const SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// 一档转录的单事件字符上限。
#[derive(Debug, Clone, Copy)]
struct TranscriptCaps {
    user: usize,
    assistant: usize,
    tool_result: usize,
}

/// 逐级收紧的上限：先用第一档，装不下就往下退。
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

/// 宿主端口：压缩策略需要而插件不该知道的五件事。
///
/// `compact_history` 的**互斥锁由宿主持有**（在调用本函数之前拿），所以这里
/// 不需要锁——守卫活在宿主的栈帧上，插件 future 被 await 的整段时间它都
/// 有效。
pub trait CompactionHost: Send + Sync {
    /// 压缩预览（日志自己算：谁会被替换、替换多少）。
    fn preview(&self, keep_recent: usize) -> Result<CompactPreview, String>;
    /// 归档当前会话快照（失败不阻塞压缩，调用方收到 `None` 并在结果里注明）。
    fn archive_snapshot(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>;
    /// 当前 provider（摘要用）。
    fn provider(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Arc<dyn LlmProvider>> + Send + '_>>;
    /// 进度提示（"正在生成摘要…"）。用户等的是这个，不是终态。
    fn progress(&self, message: String);
    /// 落地（追加 `CompactionEvent` 并返回统计）。
    fn apply<'a>(
        &'a self,
        keep_recent: usize,
        summaries: Vec<CompactionSummary>,
        archive: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<echo_session::CompactionApplied, String>>
                + Send
                + 'a,
        >,
    >;
}

/// 压缩历史：**预览 → 归档快照 → 逐组摘要 → 落地**。
///
/// 返回面向用户的一行结果（几条 / 几组 / LLM 与回退各几组 / 归档路径）。
/// 调用方负责先拿压缩互斥锁（见 [`CompactionHost`] 的说明）。
pub async fn compact_history(
    host: &dyn CompactionHost,
    keep_recent: usize,
) -> Result<String, String> {
    let preview = host.preview(keep_recent)?;
    let provider = host.provider().await;
    let archive = match host.archive_snapshot().await {
        Ok(path) => Some(path),
        Err(error) => {
            tracing::warn!(%error, "compaction: archive snapshot failed; continuing");
            None
        }
    };
    host.progress(format!(
        "正在压缩历史：{} 组、{} 条事件，生成 LLM 摘要中…",
        preview.groups.len(),
        preview.total_replaced
    ));
    let (summaries, llm_count, fallback_count) =
        summarize_preview(&provider, &preview, keep_recent).await;
    // 预览切片（事件条数）：落地只替换这批前缀，窗口期新到事件全部留在
    // 保留尾（2026-10 审计修复）。
    let prepared: Vec<CompactionSummary> = summaries
        .iter()
        .map(|summary| CompactionSummary {
            session: summary.session.clone(),
            summary: summary.summary.clone(),
            replaced_count: preview
                .groups
                .iter()
                .find(|group| group.session == summary.session)
                .map(|group| group.replaced_count)
                .unwrap_or(0),
        })
        .collect();
    let applied = host
        .apply(keep_recent, prepared, archive.as_deref())
        .await?;
    let mut message = format!(
        "已压缩 {} 条历史事件（{} 组：LLM 摘要 {} 组 / 规则回退 {} 组），保留最近 {} 条",
        applied.total_replaced, applied.groups, llm_count, fallback_count, applied.kept
    );
    if fallback_count > 0 {
        let mut reasons: Vec<String> = summaries
            .iter()
            .filter(|summary| !summary.from_llm)
            .filter_map(|summary| summary.fallback_reason.clone())
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

/// 逐组摘要（按序，每组一次调用）。**整体永不失败**：某组的 LLM 调用出错就
/// 用规则文案顶上。返回 `(摘要, LLM 组数, 回退组数)`。
pub async fn summarize_preview(
    provider: &Arc<dyn LlmProvider>,
    preview: &CompactPreview,
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

/// 一份准备好的摘要及其来源（LLM 还是规则回退）。
#[derive(Debug, Clone)]
pub struct SummaryOrFallback {
    /// 归属会话（`None` = 无归属的旧事件）。
    pub session: Option<String>,
    /// 摘要正文（不带 `[历史摘要]` 标记——投影时才加）。
    pub summary: String,
    /// 是否来自 LLM（`false` = 规则回退）。
    pub from_llm: bool,
    /// 回退原因（仅 `from_llm == false` 时有值，用于给用户的报告）。
    pub fallback_reason: Option<String>,
}

/// 用模型摘要一组。`Err` 携带短原因（同时用于回退文案与给运维的报告）。
pub async fn summarize_group(
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

/// 模型不可用时的规则摘要。保持既有统计形状，让新老压缩读起来一致。
pub fn rule_fallback_summary(
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

/// 把被替换的事件渲染成带角色标签的转录，逐级收缩直到装进
/// [`TRANSCRIPT_TOKEN_BUDGET`]：先收紧单事件上限，仍超就丢最老的事件
/// （新上下文优先）。
pub fn render_transcript(events: &[SessionEvent]) -> String {
    for caps in TRANSCRIPT_TIERS {
        let text = render_with_caps(events, caps);
        if estimate_tokens(&text) <= TRANSCRIPT_TOKEN_BUDGET {
            return text;
        }
    }
    // 最后手段：只留有最新的一批。丢前缀使转录单调收缩，所以二分找截断点，
    // 不必每丢一条就重渲染（O(n log n) vs O(n²)）。
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
    // 病态情况：单条事件就超过整个预算——硬截断。
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
    // 已被某条 assistant 消息列举过的工具调用 id。
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
                // 通常已被其 assistant 消息列举（日志的常见形状）；只有
                // 独立出现的调用才独占一行。
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
            // 推理**不进**压缩转录（2026-10）：它是显示层内容，对"这段
            // 对话讲了什么"没有信息量，却占满 transcript 预算（推理体积
            // 通常是正文的数倍）。与既有行为一致——assistant 消息的
            // `reasoning_content` 同样从不渲染。
            SessionEvent::Reasoning(_) => {}
            // 模型往返观测（2026-10-19）：token/耗时/步号对"这段对话讲了
            // 什么"没有信息量，与推理同口径不进压缩转录。
            SessionEvent::ModelStep(_) => {}
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

/// 按字符边界安全截断，带显式省略标记。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}…（截断）")
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_session::event::{AssistantMessage, UserMessage};

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

    fn group(events: Vec<SessionEvent>) -> CompactGroup {
        let count = events.len();
        CompactGroup {
            session: None,
            replaced: events,
            replaced_count: count,
            users: 2,
            tools: 0,
        }
    }

    /// 角色标签齐全，推理与模型观测不出现。
    #[test]
    fn transcript_tags_roles_and_skips_reasoning() {
        let events = vec![
            user("你叫什么"),
            SessionEvent::Reasoning(echo_session::event::ReasoningEvent {
                content: "内部思考不该出现".into(),
                session: None,
                timestamp: 0,
            }),
            assistant("我是 Echo"),
        ];
        let text = render_transcript(&events);
        assert!(text.contains("[用户] 你叫什么"), "{text}");
        assert!(text.contains("[助手] 我是 Echo"), "{text}");
        assert!(!text.contains("内部思考"), "推理不该进转录：{text}");
    }

    /// 多字节安全：按字符截断，不留半个汉字。
    #[test]
    fn truncate_is_multibyte_safe() {
        let text = truncate_chars("中文中文中文", 2);
        assert_eq!(text, "中文…（截断）");
    }

    /// 规则回退保留既有统计形状（新老压缩读起来一致）。
    #[test]
    fn rule_fallback_keeps_the_statistics_shape() {
        let text = rule_fallback_summary(&group(vec![user("a")]), 10, "provider 缺失");
        assert!(text.contains("已压缩 1 条历史事件"), "{text}");
        assert!(text.contains("2 条用户消息"), "{text}");
        assert!(text.contains("保留最近 10 条"), "{text}");
        assert!(text.contains("provider 缺失"), "{text}");
    }

    /// 空组：不调 LLM，直接回退（避免无意义的往返）。
    #[tokio::test]
    async fn empty_group_falls_back_without_calling_the_model() {
        struct PanicProvider;
        #[async_trait::async_trait]
        impl LlmProvider for PanicProvider {
            fn name(&self) -> &str {
                "panic"
            }
            fn default_model(&self) -> &str {
                "panic"
            }
            async fn chat(
                &self,
                _request: &ChatRequest,
            ) -> Result<echo_defs::message::ChatResponse, echo_defs::llm::LlmError> {
                panic!("空组不该调用模型")
            }
            async fn chat_stream(
                &self,
                _request: &ChatRequest,
                _tx: tokio::sync::mpsc::UnboundedSender<echo_defs::message::ChatChunk>,
            ) -> Result<echo_defs::message::ChatResponse, echo_defs::llm::LlmError> {
                panic!("空组不该调用模型")
            }
        }
        let provider: Arc<dyn LlmProvider> = Arc::new(PanicProvider);
        let result = summarize_group(&provider, &group(vec![])).await;
        assert!(result.is_err(), "空组必须回退而不是成功");
    }

    /// 转录超预算时按 tier 收缩，并且**保留最新**（丢最老）。
    #[test]
    fn transcript_shrinks_and_keeps_the_newest() {
        let mut events = Vec::new();
        for index in 0..200 {
            events.push(user(&format!("第{index}条：{}", "x".repeat(2000))));
        }
        let text = render_transcript(&events);
        assert!(
            text.contains("第199条"),
            "最新的必须保留（转录开头是省略标记）"
        );
        assert!(
            text.contains("更早记录已省略"),
            "丢前缀时必须显式标注：{}",
            &text[..text.len().min(80)]
        );
    }
}
