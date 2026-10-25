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
use echo_session::{
    CompactGroup, CompactPreview, CompactionPhase, CompactionProgress, CompactionSummary,
};

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
    /// 摘要失败的**备用出口**（2026-10 修复：人格 provider 欠费/不可用时
    /// 摘要全组降级为统计文案，上下文直接丢失）。
    ///
    /// 宿主按「全局默认 profile（`active_api`）」解析；与 [`Self::provider`]
    /// 等价时应返回 `None`（重试同一个出口没有意义）。默认实现返回 `None`
    /// ——不认识全局默认的宿主保持原行为。
    fn fallback_provider(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<Arc<dyn LlmProvider>>> + Send + '_>,
    > {
        Box::pin(async { None })
    }
    /// 进度上报（阶段 + 分子分母 + 一句话）。
    ///
    /// 用户等的是**这个**，不是终态：压缩逐会话调 LLM，可能跑几分钟。宿主
    /// 把它翻成线协议事件（`BackendEvent::CompactionProgress`），面板据此画
    /// 进度条。默认实现丢弃 —— 嵌入式宿主不需要进度。
    ///
    /// 约定：必须是**廉价且不阻塞**的（`compact_history` 在每次 LLM 调用前后
    /// 都会调它）；宿主自己负责节流/丢弃。
    fn progress(&self, _progress: &CompactionProgress) {}

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
    host.progress(&CompactionProgress::step(
        CompactionPhase::Preview,
        "正在计算压缩计划…",
    ));
    let preview = host.preview(keep_recent)?;
    let groups = preview.groups.len();
    host.progress(&CompactionProgress::counted(
        CompactionPhase::Archive,
        0,
        0,
        groups,
        format!(
            "正在归档快照（{groups} 个会话、{} 条事件将被压缩）…",
            preview.total_replaced
        ),
    ));
    let provider = host.provider().await;
    let archive = match host.archive_snapshot().await {
        Ok(path) => Some(path),
        Err(error) => {
            tracing::warn!(%error, "compaction: archive snapshot failed; continuing");
            None
        }
    };
    host.progress(&CompactionProgress::counted(
        CompactionPhase::Summarize,
        0,
        groups,
        groups,
        format!("开始生成摘要（共 {groups} 组）…"),
    ));
    // 摘要备用出口（2026-10 修复）：人格 profile 欠费（HTTP 402）或端点不可用
    // 时，此前整份历史只剩统计文案——损失比压缩本身大得多。取全局默认 profile
    // 的 provider 作备用，逐组失败时重试一次。
    let fallback = host.fallback_provider().await;
    // 逐组进度：**每完成一组就报一次**——这是唯一可能持续数分钟的阶段，
    // 进度条的价值全在这里。
    let total_groups = preview.groups.len();
    let reporter = |done: usize, total: usize, session: Option<&str>| {
        let who = session.map(|s| format!("（{s}）")).unwrap_or_default();
        host.progress(&CompactionProgress::counted(
            CompactionPhase::Summarize,
            done,
            total,
            total,
            format!("正在生成摘要 {done}/{total}{who}…"),
        ));
    };
    let (summaries, llm_count, fallback_count) = summarize_preview_with_progress(
        &provider,
        fallback.as_ref(),
        &preview,
        keep_recent,
        &reporter,
    )
    .await;
    debug_assert!(total_groups >= llm_count);
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
    host.progress(&CompactionProgress::counted(
        CompactionPhase::Apply,
        groups,
        groups,
        groups,
        "摘要已全部生成，正在写入历史…",
    ));
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
    host.progress(&CompactionProgress::counted(
        CompactionPhase::Done,
        applied.groups,
        applied.groups,
        applied.groups,
        message.clone(),
    ));
    Ok(message)
}

/// 逐组摘要（按序，每组**最多两次**调用）。**整体永不失败**：主出口出错时
/// 先用 `fallback`（全局默认 profile）重试一次，仍失败才用规则文案顶上。
/// 返回 `(摘要, LLM 组数, 回退组数)`——备用出口成功的计入 LLM 组数。
pub async fn summarize_preview(
    provider: &Arc<dyn LlmProvider>,
    fallback: Option<&Arc<dyn LlmProvider>>,
    preview: &CompactPreview,
    keep_recent: usize,
) -> (Vec<SummaryOrFallback>, usize, usize) {
    summarize_preview_with_progress(
        provider,
        fallback,
        preview,
        keep_recent,
        &|_done, _total, _session| {},
    )
    .await
}

/// 同 [`summarize_preview`]，但**每完成一组**就回调一次进度
/// `(done, total, 会话)`。`progress` 必须是廉价操作（见
/// [`CompactionHost::progress`] 的说明）。
pub async fn summarize_preview_with_progress(
    provider: &Arc<dyn LlmProvider>,
    fallback: Option<&Arc<dyn LlmProvider>>,
    preview: &CompactPreview,
    keep_recent: usize,
    progress: &(dyn Fn(usize, usize, Option<&str>) + Sync),
) -> (Vec<SummaryOrFallback>, usize, usize) {
    let mut out = Vec::new();
    let mut llm_count = 0usize;
    let mut fallback_count = 0usize;
    let total = preview.groups.len();
    for (index, group) in preview.groups.iter().enumerate() {
        let primary = summarize_group(provider, group).await;
        let mut retried = false;
        let (summary, from_llm, fallback_reason) = match primary {
            Ok(body) => (body, true, None),
            Err(primary_reason) => {
                // 2026-10 修复：人格 profile 欠费（HTTP 402）/ 端点不可用时，
                // 此前整组直接降级为统计文案，历史信息全丢。这里换全局默认
                // 出口重试一次；两次失败原因都留在回退文案里便于排障。
                let retry = match fallback {
                    Some(alternative) => {
                        retried = true;
                        summarize_group(alternative, group)
                            .await
                            .map_err(|retry_reason| {
                                format!("{primary_reason}；备用出口重试亦失败：{retry_reason}")
                            })
                    }
                    None => Err(primary_reason),
                };
                match retry {
                    Ok(body) => {
                        tracing::warn!(
                            session = group.session.as_deref().unwrap_or("<legacy>"),
                            "compaction: primary provider failed, fallback provider succeeded"
                        );
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
                }
            }
        };
        if from_llm {
            llm_count += 1;
        }
        tracing::info!(
            session = group.session.as_deref().unwrap_or("<legacy>"),
            replaced = group.replaced_count,
            from_llm,
            retried,
            "compaction: group summarized"
        );
        out.push(SummaryOrFallback {
            session: group.session.clone(),
            summary,
            from_llm,
            fallback_reason,
        });
        // 本组已落地（无论 LLM 还是规则回退）：进度**照样前进**——回退也是
        // 完成了一组，让进度条停住只会让人以为卡了。
        progress(index + 1, total, group.session.as_deref());
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
pub fn rule_fallback_summary(group: &CompactGroup, keep_recent: usize, reason: &str) -> String {
    format!(
        "⚠️ 自动摘要失败，仅保留统计信息：已压缩 {} 条历史事件（{} 条用户消息，\
         {} 次工具调用），保留最近 {} 条。失败原因：{}。\
         ——上下文细节已丢失，需要时请从归档快照恢复。",
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
            timestamp: 0,
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

    /// 可编排的 provider：`fail = true` 时按 HTTP 402 报错（真实故障形状）。
    struct ScriptedProvider {
        reply: &'static str,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }
        fn default_model(&self) -> &str {
            "scripted-model"
        }
        async fn chat(
            &self,
            _request: &ChatRequest,
        ) -> Result<echo_defs::message::ChatResponse, echo_defs::llm::LlmError> {
            if self.fail {
                return Err(echo_defs::llm::LlmError::Api(
                    "API 返回错误: HTTP 402 Payment Required: Insufficient Balance".into(),
                ));
            }
            let mut response = echo_defs::message::ChatResponse::empty();
            response.content = Some(self.reply.to_string());
            Ok(response)
        }
        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<echo_defs::message::ChatChunk>,
        ) -> Result<echo_defs::message::ChatResponse, echo_defs::llm::LlmError> {
            self.chat(_request).await
        }
    }

    /// 进度序列：**每组都报一次**，且回退组也前进（回退也是完成）。
    /// 让进度条停住只会让人以为卡了。
    #[tokio::test]
    async fn progress_advances_for_every_group_including_fallbacks() {
        use std::sync::Mutex;

        // 记录进度回调：(done, total)
        let seen: Arc<Mutex<Vec<(usize, usize)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            reply: "",
            fail: true,
        });

        let preview = CompactPreview {
            groups: vec![
                group(vec![user("a"), assistant("b")]),
                group(vec![user("c"), assistant("d")]),
                group(vec![user("e"), assistant("f")]),
            ],
            total_replaced: 6,
            total_events: 9,
        };
        let (summaries, llm, fallback) = summarize_preview_with_progress(
            &provider,
            None,
            &preview,
            1,
            &|done, total, _session| sink.lock().unwrap().push((done, total)),
        )
        .await;
        assert_eq!(summaries.len(), 3);
        assert_eq!((llm, fallback), (0, 3), "全部走规则回退");
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(1, 3), (2, 3), (3, 3)],
            "三组各报一次，分子递增到 total"
        );
    }

    /// 主出口失败、备用出口成功时，进度同样只前进一次（不能因"重试"多报）。
    #[tokio::test]
    async fn retry_on_fallback_counts_as_one_step() {
        use std::sync::Mutex;
        let seen: Arc<Mutex<Vec<(usize, usize)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let primary: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            reply: "",
            fail: true,
        });
        let fallback: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            reply: "【历史摘要】ok",
            fail: false,
        });
        let preview = CompactPreview {
            groups: vec![group(vec![user("a"), assistant("b")])],
            total_replaced: 2,
            total_events: 3,
        };
        let (_summaries, llm, fallback_count) = summarize_preview_with_progress(
            &primary,
            Some(&fallback),
            &preview,
            1,
            &|done, total, _session| sink.lock().unwrap().push((done, total)),
        )
        .await;
        assert_eq!((llm, fallback_count), (1, 0), "备用出口成功计入 LLM 组");
        assert_eq!(*seen.lock().unwrap(), vec![(1, 1)], "一组只报一次进度");
    }

    fn preview_of(events: Vec<SessionEvent>) -> CompactPreview {
        let group = group(events);
        CompactPreview {
            total_replaced: group.replaced_count,
            total_events: group.replaced_count,
            groups: vec![group],
        }
    }

    /// 主出口欠费 → 用备用出口重试并成功（2026-10 修复：此前整组降级为
    /// 统计文案，历史信息全丢——「聊天还能用、一压缩就失忆」的根因）。
    #[tokio::test]
    async fn failing_primary_provider_retries_on_fallback() {
        let primary: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            reply: "",
            fail: true,
        });
        let fallback: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            reply: "## 当前目标\n恢复上下文",
            fail: false,
        });
        let (summaries, llm_count, fallback_count) = summarize_preview(
            &primary,
            Some(&fallback),
            &preview_of(vec![user("继续排查")]),
            10,
        )
        .await;
        assert_eq!(llm_count, 1, "备用出口成功应计入 LLM 组数");
        assert_eq!(fallback_count, 0, "不应记为规则回退");
        assert!(summaries[0].from_llm);
        assert!(summaries[0].summary.contains("恢复上下文"));
    }

    /// 两个出口都失败 → 规则回退，且文案里两次原因都在（排障要能看出
    /// 「主出口欠费、备用出口也连不上」）。
    #[tokio::test]
    async fn both_providers_failing_keeps_both_reasons() {
        let primary: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            reply: "",
            fail: true,
        });
        let fallback: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            reply: "",
            fail: true,
        });
        let (summaries, llm_count, fallback_count) = summarize_preview(
            &primary,
            Some(&fallback),
            &preview_of(vec![user("继续排查")]),
            10,
        )
        .await;
        assert_eq!(llm_count, 0);
        assert_eq!(fallback_count, 1);
        assert!(!summaries[0].from_llm);
        let reason = summaries[0].fallback_reason.as_deref().unwrap_or_default();
        assert!(reason.contains("402"), "主出口原因缺失：{reason}");
        assert!(
            reason.contains("备用出口重试亦失败"),
            "备用原因缺失：{reason}"
        );
    }

    /// 没有备用出口时行为与修复前一致（默认实现返回 None）。
    #[tokio::test]
    async fn without_fallback_primary_failure_still_falls_back() {
        let primary: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            reply: "",
            fail: true,
        });
        let (summaries, llm_count, fallback_count) =
            summarize_preview(&primary, None, &preview_of(vec![user("a")]), 10).await;
        assert_eq!(llm_count, 0);
        assert_eq!(fallback_count, 1);
        assert!(summaries[0].summary.contains("已压缩 1 条历史事件"));
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
