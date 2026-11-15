//! 上下文压缩**策略**：把被替换的事件变成一份交接摘要。
//!
//! ## 为什么压缩策略归 loop 模块
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
//! | 摘要提示词与单组调用（超时 / 空输出 / 截断 / 工具调用异常 / 错误） | provider 从哪来 |
//! | 失败组的隔离与报告（不装残片、不用统计文案顶替） | 归档快照（写 `archives/`） |
//! | 结果文案（几条 / 几组 / 未压缩原因） | 落地（追加 `CompactionEvent`）+ 进度事件 |
//!
//! ## 失败安全（PR1，2026-11）
//!
//! 压缩会改写上下文，所以每一步失败都必须**朝保留历史的方向**收：
//!
//! 1. **归档失败即中止**：没有快照就不改写历史；memory-only 部署同样
//!    fail-closed，不提供"跳过归档"的隐式有损开关。
//! 2. **异常摘要即拒绝**：非空但被 token 上限截断、或响应里冒出工具调用
//!    （请求没给工具）的摘要都是残片，不能安装。
//! 3. **组级隔离**：主出口失败先重试备用出口；两个出口都失败的组不进入
//!    落地摘要（**绝不用统计文案冒充摘要**），其余组照常压缩并如实报告。
//! 4. **全失败不改历史**：所有组都失败时整体 `Err`，不调用 apply。
//!
//! ## 渲染不变量
//!
//! **转录按预算分层收缩**：先收紧每事件字符上限（三档），仍超预算就丢最老
//! 的事件（新上下文优先）——用二分找截断点，不是逐条重渲染（O(n log n)）。
//!
//! 另外两条渲染口径：**推理与模型往返观测不进转录**（它们对"这段对话讲了
//! 什么"没有信息量，却占满预算）；**工具调用若已被 assistant 消息列举就
//! 不重复成行**（日志的常见形状是"消息带 tool_calls + 独立 ToolCall 事件"）。

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

/// 宿主端口：压缩策略需要而模块不该知道的六件事。
///
/// `compact_history` 的**互斥锁由宿主持有**（在调用本函数之前拿），所以这里
/// 不需要锁——守卫活在宿主的栈帧上，模块 future 被 await 的整段时间它都
/// 有效。
pub trait CompactionHost: Send + Sync {
    /// 压缩预览（日志自己算：谁会被替换、替换多少）。
    fn preview(&self, keep_recent: usize) -> Result<CompactPreview, String>;
    /// 归档当前会话快照。**失败即中止压缩（fail-closed）**：没有快照就不
    /// 改写历史；memory-only 部署没有可归档文件，同样按失败处理（不提供
    /// 隐式有损开关）。
    fn archive_snapshot(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>;
    /// 当前 provider（摘要用）。**只在归档成功之后获取**。
    fn provider(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Arc<dyn LlmProvider>> + Send + '_>>;
    /// 摘要失败的**备用出口**（2026-10 修复：人格 provider 欠费/不可用时
    /// 整份历史降级成统计文案，上下文直接丢失）。
    ///
    /// 主出口失败的组用备用出口重试一次；仍失败则该组不安装（PR1）。
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
/// 失败安全（PR1）：
/// - 归档失败立即 `Err`：不取 provider、不调 LLM、不改历史（memory-only
///   同样 fail-closed，没有隐式有损开关）；
/// - 主备两个出口都失败的组**不进入落地摘要**（不用统计文案替换），其余
///   组照常压缩并如实报告；
/// - 全部组失败时整体 `Err`，不调用 [`CompactionHost::apply`]。
///
/// 返回面向用户的一行结果（几条 / 几组 / LLM 与未压缩各几组 / 归档路径）。
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
    // 失败安全（PR1）：归档是压缩的前置条件——没有快照就不改写历史。
    // 顺序也重要：先归档成功，再获取 provider（provider 可能慢/计费）。
    let archive = host.archive_snapshot().await.map_err(|error| {
        tracing::warn!(
            %error,
            "compaction: archive snapshot failed; aborting (fail-closed)"
        );
        format!("归档快照失败，已中止压缩（历史未改动）：{error}")
    })?;
    let provider = host.provider().await;
    host.progress(&CompactionProgress::counted(
        CompactionPhase::Summarize,
        0,
        groups,
        groups,
        format!("开始生成摘要（共 {groups} 组）…"),
    ));
    // 摘要备用出口（2026-10 修复）：人格 profile 欠费（HTTP 402）或端点不可用
    // 时，整份历史会只剩统计文案。取全局默认 profile 的 provider 作备用，
    // 逐组失败时重试一次；仍失败则该组**不安装**（PR1，不再用统计文案替换）。
    let fallback = host.fallback_provider().await;
    // 逐组进度：**每完成一组就报一次**——这是唯一可能持续数分钟的阶段，
    // 进度条的价值全在这里。
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
    let (summaries, llm_count, failed_count) = summarize_preview_with_progress(
        &provider,
        fallback.as_ref(),
        &preview,
        keep_recent,
        &reporter,
    )
    .await;
    debug_assert_eq!(groups, llm_count + failed_count);
    if llm_count == 0 {
        // 全失败：返回 Err 且**不 apply**——历史保持原样，用户看到真实原因。
        return Err(format!(
            "{groups} 组摘要全部失败，历史未改动（未安装任何摘要）。失败原因：{}",
            failure_reasons(&summaries).join("；")
        ));
    }
    // 只把**成功**的摘要变成落地摘要：失败组不进入 prepared（PR1）。
    // 凭据（前缀指纹 / 历史代次）由 `from_group` 从预览组原样复制。
    let prepared: Vec<CompactionSummary> = summaries
        .iter()
        .filter(|summary| summary.from_llm)
        .filter_map(|summary| {
            preview
                .groups
                .iter()
                .find(|group| group.session == summary.session)
                .map(|group| CompactionSummary::from_group(group, summary.summary.clone()))
        })
        .collect();
    debug_assert_eq!(
        prepared.len(),
        llm_count,
        "每个成功摘要都必须对应一个预览组"
    );
    host.progress(&CompactionProgress::counted(
        CompactionPhase::Apply,
        groups,
        groups,
        groups,
        "摘要生成结束，正在写入历史…",
    ));
    let applied = host.apply(keep_recent, prepared, Some(&archive)).await?;
    // 落地时 store 还会按前缀指纹/代次校验；被拒绝的组原样保留，报告出来。
    let rejected = llm_count.saturating_sub(applied.groups);
    let mut message = format!(
        "已压缩 {} 条历史事件（{}/{} 组：LLM 摘要 {} 组 / 未压缩 {} 组），保留最近 {} 条",
        applied.total_replaced, applied.groups, groups, llm_count, failed_count, applied.kept
    );
    if failed_count > 0 {
        message.push_str(&format!(
            "；未压缩组原因：{}",
            failure_reasons(&summaries).join("；")
        ));
    }
    if rejected > 0 {
        message.push_str(&format!(
            "；另有 {rejected} 组在落地时因历史变化被拒绝（原事件保留）"
        ));
    }
    message.push_str(&format!("；原事件已归档：{archive}"));
    host.progress(&CompactionProgress::counted(
        CompactionPhase::Done,
        applied.groups,
        groups,
        groups,
        message.clone(),
    ));
    Ok(message)
}

/// 失败组的去重原因（供整体错误与混合结果文案共用）。
fn failure_reasons(summaries: &[SummaryOrFallback]) -> Vec<String> {
    let mut reasons: Vec<String> = summaries
        .iter()
        .filter_map(|summary| summary.failure_reason.clone())
        .collect();
    reasons.dedup();
    reasons
}

/// 逐组摘要（按序，每组**最多两次**调用：主出口 + 备用出口）。
///
/// 单组失败**不再**生成统计文案（PR1：那等于用假摘要换掉真历史）：
/// `from_llm == false` 的条目 `summary` 为空，`failure_reason` 记录主备
/// 两次失败的原因，由 [`compact_history`] 决定隔离该组还是整体失败。
/// 返回 `(组结果, LLM 组数, 失败组数)`——备用出口成功的计入 LLM 组数。
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
    _keep_recent: usize,
    progress: &(dyn Fn(usize, usize, Option<&str>) + Sync),
) -> (Vec<SummaryOrFallback>, usize, usize) {
    let mut out = Vec::new();
    let mut llm_count = 0usize;
    let mut failed_count = 0usize;
    let total = preview.groups.len();
    for (index, group) in preview.groups.iter().enumerate() {
        let primary = summarize_group(provider, group).await;
        let mut retried = false;
        let (summary, from_llm, failure_reason) = match primary {
            Ok(body) => (body, true, None),
            Err(primary_reason) => {
                // 2026-10 修复：人格 profile 欠费（HTTP 402）/ 端点不可用时，
                // 此前整组直接降级为统计文案，历史信息全丢。这里换全局默认
                // 出口重试一次；两次失败原因都留在失败原因里便于排障。
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
                        failed_count += 1;
                        // 失败组不生成摘要（PR1）：绝不用统计文案替换历史。
                        (String::new(), false, Some(reason))
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
            failure_reason,
        });
        // 本组已尝试完成（成功或失败）：进度**照样前进**——让进度条停住
        // 只会让人以为卡了；整体失败由 compact_history 返回 Err。
        progress(index + 1, total, group.session.as_deref());
    }
    (out, llm_count, failed_count)
}

/// 一份组级结果：成功（LLM / 备用出口）或失败（两个出口都失败）。
///
/// 名字沿用旧 API；`from_llm == false` 时**没有摘要**——不再有"规则回退
/// 文案"这种东西（PR1）。
#[derive(Debug, Clone)]
pub struct SummaryOrFallback {
    /// 归属会话（`None` = 无归属的旧事件）。
    pub session: Option<String>,
    /// 摘要正文（不带 `[历史摘要]` 标记——投影时才加）；失败时为空。
    pub summary: String,
    /// 是否拿到可安装的摘要（主出口或备用出口成功）。
    pub from_llm: bool,
    /// 失败原因（仅 `from_llm == false` 时有值，用于给用户的报告）。
    pub failure_reason: Option<String>,
}

/// 用模型摘要一组。`Err` 携带短原因（同时用于给用户的报告）。
///
/// 只接受**完整纯文本**：非空但被 token 上限截断（`stop_reason` 为
/// `max_tokens` / `length`）、或响应里带工具调用（请求根本没给工具）都算
/// 残片，一律拒绝（PR1）。
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
    if response.truncated() {
        return Err("摘要输出被截断（达到 token 上限，只有残片）".into());
    }
    if !response.tool_calls.is_empty() {
        return Err(format!(
            "摘要响应包含 {} 个工具调用（应为纯文本）",
            response.tool_calls.len()
        ));
    }
    let body = response.content.unwrap_or_default();
    let body = body.trim();
    if body.is_empty() {
        return Err("摘要输出为空".into());
    }
    Ok(body.to_string())
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
            files: vec![],
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
            prefix_digest: echo_session::CompactionPrefix::of(&events),
            history_epoch: 0,
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

    /// 失败组不产出统计摘要（PR1）：组结果里 summary 为空，只有失败原因。
    #[tokio::test]
    async fn failed_group_has_no_summary_body() {
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            reply: "",
            fail: true,
        });
        let (summaries, llm, failed) =
            summarize_preview(&provider, None, &preview_of(vec![user("a")]), 10).await;
        assert_eq!((llm, failed), (0, 1));
        assert!(!summaries[0].from_llm);
        assert!(
            summaries[0].summary.is_empty(),
            "失败组不得用统计文案冒充摘要：{}",
            summaries[0].summary
        );
        assert!(summaries[0].failure_reason.is_some());
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

    /// 进度序列：**每组都报一次**，失败组也前进（尝试过也算完成）。
    /// 让进度条停住只会让人以为卡了；整体失败由 compact_history 返回 Err。
    #[tokio::test]
    async fn progress_advances_for_every_group_including_failures() {
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
        let (summaries, llm, failed) = summarize_preview_with_progress(
            &provider,
            None,
            &preview,
            1,
            &|done, total, _session| sink.lock().unwrap().push((done, total)),
        )
        .await;
        assert_eq!(summaries.len(), 3);
        assert_eq!((llm, failed), (0, 3), "三组全部失败（无摘要兜底）");
        assert!(summaries.iter().all(|summary| summary.summary.is_empty()));
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
        let (_summaries, llm, failed_count) = summarize_preview_with_progress(
            &primary,
            Some(&fallback),
            &preview,
            1,
            &|done, total, _session| sink.lock().unwrap().push((done, total)),
        )
        .await;
        assert_eq!((llm, failed_count), (1, 0), "备用出口成功计入 LLM 组");
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
        let (summaries, llm_count, failed_count) = summarize_preview(
            &primary,
            Some(&fallback),
            &preview_of(vec![user("继续排查")]),
            10,
        )
        .await;
        assert_eq!(llm_count, 1, "备用出口成功应计入 LLM 组数");
        assert_eq!(failed_count, 0, "备用出口成功不算失败组");
        assert!(summaries[0].from_llm);
        assert!(summaries[0].summary.contains("恢复上下文"));
    }

    /// 两个出口都失败 → 失败组（**无摘要正文**），主备两次原因都要在
    /// （排障要能看出「主出口欠费、备用出口也连不上」）。
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
        let (summaries, llm_count, failed_count) = summarize_preview(
            &primary,
            Some(&fallback),
            &preview_of(vec![user("继续排查")]),
            10,
        )
        .await;
        assert_eq!(llm_count, 0);
        assert_eq!(failed_count, 1);
        assert!(!summaries[0].from_llm);
        assert!(summaries[0].summary.is_empty(), "失败组不得有摘要正文");
        let reason = summaries[0].failure_reason.as_deref().unwrap_or_default();
        assert!(reason.contains("402"), "主出口原因缺失：{reason}");
        assert!(
            reason.contains("备用出口重试亦失败"),
            "备用原因缺失：{reason}"
        );
    }

    /// 没有备用出口时主出口失败 = 失败组（无摘要），原因仍可上报。
    #[tokio::test]
    async fn without_fallback_primary_failure_is_a_failed_group() {
        let primary: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            reply: "",
            fail: true,
        });
        let (summaries, llm_count, failed_count) =
            summarize_preview(&primary, None, &preview_of(vec![user("a")]), 10).await;
        assert_eq!(llm_count, 0);
        assert_eq!(failed_count, 1);
        assert!(summaries[0].summary.is_empty());
        assert!(summaries[0]
            .failure_reason
            .as_deref()
            .unwrap_or_default()
            .contains("402"));
    }

    /// 空组：不调 LLM，直接按失败处理（避免无意义的往返）。
    #[tokio::test]
    async fn empty_group_fails_without_calling_the_model() {
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
        assert!(result.is_err(), "空组必须按失败处理而不是成功");
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

    // ── PR1 失败安全回归：归档失败中止 / 组级隔离 / 异常摘要拒绝 ──

    use echo_session::CompactionApplied;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 某会话的一组（`session` 是落地匹配键，测试里必须可区分）。
    fn session_group(session: &str, events: Vec<SessionEvent>) -> CompactGroup {
        let mut group = group(events);
        group.session = Some(session.to_string());
        group
    }

    /// 返回**完整响应**的 provider：可造截断（`length`）与工具调用等异常形状。
    struct CannedProvider {
        response: echo_defs::message::ChatResponse,
        calls: Arc<AtomicUsize>,
    }

    impl CannedProvider {
        fn new(response: echo_defs::message::ChatResponse) -> Self {
            Self {
                response,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for CannedProvider {
        fn name(&self) -> &str {
            "canned"
        }
        fn default_model(&self) -> &str {
            "canned-model"
        }
        async fn chat(
            &self,
            _request: &ChatRequest,
        ) -> Result<echo_defs::message::ChatResponse, echo_defs::llm::LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.response.clone())
        }
        async fn chat_stream(
            &self,
            request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<echo_defs::message::ChatChunk>,
        ) -> Result<echo_defs::message::ChatResponse, echo_defs::llm::LlmError> {
            self.chat(request).await
        }
    }

    fn canned(content: &str) -> echo_defs::message::ChatResponse {
        let mut response = echo_defs::message::ChatResponse::empty();
        response.content = Some(content.to_string());
        response
    }

    /// 按转录内容路由：含「失败组」的调用报 402（真实故障形状），其余成功。
    struct RoutingProvider;

    #[async_trait::async_trait]
    impl LlmProvider for RoutingProvider {
        fn name(&self) -> &str {
            "routing"
        }
        fn default_model(&self) -> &str {
            "routing-model"
        }
        async fn chat(
            &self,
            request: &ChatRequest,
        ) -> Result<echo_defs::message::ChatResponse, echo_defs::llm::LlmError> {
            let text: String = request
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if text.contains("失败组") {
                Err(echo_defs::llm::LlmError::Api(
                    "API 返回错误: HTTP 402 Payment Required: Insufficient Balance".into(),
                ))
            } else {
                Ok(canned("【当前目标】成功组摘要"))
            }
        }
        async fn chat_stream(
            &self,
            request: &ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<echo_defs::message::ChatChunk>,
        ) -> Result<echo_defs::message::ChatResponse, echo_defs::llm::LlmError> {
            self.chat(request).await
        }
    }

    /// 可编排的假宿主：记录调用顺序 / 落地摘要 / 进度，不碰真实存储。
    struct FakeHost {
        preview: CompactPreview,
        archive: Result<String, String>,
        provider: Arc<dyn LlmProvider>,
        fallback: Option<Arc<dyn LlmProvider>>,
        steps: std::sync::Mutex<Vec<&'static str>>,
        applied: std::sync::Mutex<Vec<Vec<CompactionSummary>>>,
        progress: std::sync::Mutex<Vec<(CompactionPhase, usize, usize, String)>>,
    }

    impl FakeHost {
        fn new(
            preview: CompactPreview,
            archive: Result<String, String>,
            provider: Arc<dyn LlmProvider>,
            fallback: Option<Arc<dyn LlmProvider>>,
        ) -> Self {
            Self {
                preview,
                archive,
                provider,
                fallback,
                steps: std::sync::Mutex::new(Vec::new()),
                applied: std::sync::Mutex::new(Vec::new()),
                progress: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn steps(&self) -> Vec<&'static str> {
            self.steps.lock().unwrap().clone()
        }

        fn applied(&self) -> Vec<Vec<CompactionSummary>> {
            self.applied.lock().unwrap().clone()
        }
    }

    impl CompactionHost for FakeHost {
        fn preview(&self, _keep_recent: usize) -> Result<CompactPreview, String> {
            self.steps.lock().unwrap().push("preview");
            Ok(self.preview.clone())
        }

        fn archive_snapshot(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
        {
            self.steps.lock().unwrap().push("archive");
            let result = self.archive.clone();
            Box::pin(async move { result })
        }

        fn provider(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Arc<dyn LlmProvider>> + Send + '_>>
        {
            self.steps.lock().unwrap().push("provider");
            let provider = self.provider.clone();
            Box::pin(async move { provider })
        }

        fn fallback_provider(
            &self,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<Arc<dyn LlmProvider>>> + Send + '_>,
        > {
            self.steps.lock().unwrap().push("fallback");
            let fallback = self.fallback.clone();
            Box::pin(async move { fallback })
        }

        fn progress(&self, progress: &CompactionProgress) {
            self.progress.lock().unwrap().push((
                progress.phase,
                progress.done,
                progress.total,
                progress.message.clone(),
            ));
        }

        fn apply<'a>(
            &'a self,
            keep_recent: usize,
            summaries: Vec<CompactionSummary>,
            _archive: Option<&'a str>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<CompactionApplied, String>> + Send + 'a>,
        > {
            self.steps.lock().unwrap().push("apply");
            self.applied.lock().unwrap().push(summaries.clone());
            let applied = CompactionApplied {
                total_replaced: summaries.len(),
                groups: summaries.len(),
                kept: keep_recent,
            };
            Box::pin(async move { Ok(applied) })
        }
    }

    /// 归档失败 = 压缩失败：**不得**取 provider、不得调 LLM、不得落地。
    #[tokio::test]
    async fn archive_failure_aborts_before_any_llm_or_apply() {
        let provider = Arc::new(CannedProvider::new(canned("不应被安装的摘要")));
        let calls = Arc::clone(&provider.calls);
        let host = FakeHost::new(
            preview_of(vec![user("继续排查")]),
            Err("磁盘只读".into()),
            provider,
            None,
        );

        let error = compact_history(&host, 10)
            .await
            .expect_err("归档失败必须中止压缩（fail-closed）");
        assert!(error.contains("归档"), "错误必须说清归档失败：{error}");
        assert_eq!(
            host.steps(),
            vec!["preview", "archive"],
            "归档失败后不得再取 provider / 调 LLM / 落地"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "归档失败不得调用 LLM");
        assert!(host.applied().is_empty(), "归档失败不得落地");
    }

    /// 主备两个出口都失败的组不进入 prepared：整体 Err，且**不 apply**
    /// （不能用统计摘要替换历史）。
    #[tokio::test]
    async fn all_groups_failing_returns_err_and_never_applies() {
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            reply: "",
            fail: true,
        });
        let fallback: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            reply: "",
            fail: true,
        });
        let preview = CompactPreview {
            groups: vec![
                session_group("a", vec![user("a1")]),
                session_group("b", vec![user("b1")]),
            ],
            total_replaced: 2,
            total_events: 2,
        };
        let host = FakeHost::new(
            preview,
            Ok("archives/x-precompact.json".into()),
            provider,
            Some(fallback),
        );

        let error = compact_history(&host, 10)
            .await
            .expect_err("全部组失败必须整体 Err");
        assert!(error.contains("全部"), "错误要说明全组失败：{error}");
        assert!(error.contains("402"), "失败原因必须如实上报：{error}");
        assert!(
            host.applied().is_empty(),
            "全失败不得 apply（不许用统计摘要替换）"
        );
    }

    /// 混合结果：成功的组照常落地，失败的组**原样保留**并如实报告。
    #[tokio::test]
    async fn mixed_groups_only_apply_the_successful_summary() {
        let preview = CompactPreview {
            groups: vec![
                session_group("ok", vec![user("成功组：继续")]),
                session_group("bad", vec![user("失败组：欠费")]),
            ],
            total_replaced: 2,
            total_events: 2,
        };
        let host = FakeHost::new(
            preview,
            Ok("archives/x-precompact.json".into()),
            Arc::new(RoutingProvider),
            None,
        );

        let message = compact_history(&host, 10)
            .await
            .expect("混合结果必须保留成功组");
        let applied = host.applied();
        assert_eq!(applied.len(), 1, "只 apply 一次");
        assert_eq!(applied[0].len(), 1, "只安装成功组，失败组不得进入 prepared");
        assert!(
            applied[0][0].summary.contains("成功组摘要"),
            "安装的必须是成功摘要：{:?}",
            applied[0][0].summary
        );
        assert!(
            message.contains("未压缩 1 组"),
            "必须如实报告失败组：{message}"
        );
        assert!(message.contains("402"), "失败原因要对用户可见：{message}");
        assert!(
            message.contains("archives/x-precompact.json"),
            "归档路径要报告：{message}"
        );
    }

    /// 非空但被 token 上限截断的摘要 = 残片，必须拒绝（不能当完整摘要安装）。
    #[tokio::test]
    async fn truncated_summary_is_rejected() {
        let mut response = canned("【当前目标】写了一半就被截断");
        response.stop_reason = Some("length".into());
        let provider: Arc<dyn LlmProvider> = Arc::new(CannedProvider::new(response));
        let error = summarize_group(&provider, &group(vec![user("继续")]))
            .await
            .expect_err("非空但截断的摘要必须拒绝");
        assert!(error.contains("截断"), "{error}");
    }

    /// 摘要响应里冒出的工具调用是异常形状（请求没给工具），必须拒绝。
    #[tokio::test]
    async fn summary_with_tool_calls_is_rejected() {
        let mut response = canned("【当前目标】看起来正常");
        response.tool_calls = vec![echo_defs::message::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }];
        let provider: Arc<dyn LlmProvider> = Arc::new(CannedProvider::new(response));
        let error = summarize_group(&provider, &group(vec![user("继续")]))
            .await
            .expect_err("摘要响应带工具调用必须拒绝");
        assert!(error.contains("工具调用"), "{error}");
    }

    /// 主出口截断 + 备用出口带工具调用：两组异常都拒绝，**一个字节都不安装**。
    #[tokio::test]
    async fn truncated_or_tool_call_outputs_are_never_installed() {
        let mut truncated = canned("【当前目标】残缺");
        truncated.stop_reason = Some("max_tokens".into());
        let mut with_tools = canned("【当前目标】奇怪形状");
        with_tools.tool_calls = vec![echo_defs::message::ToolCall {
            id: "c9".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }];
        let host = FakeHost::new(
            preview_of(vec![user("继续排查")]),
            Ok("archives/x-precompact.json".into()),
            Arc::new(CannedProvider::new(truncated)),
            Some(Arc::new(CannedProvider::new(with_tools))),
        );

        let error = compact_history(&host, 10)
            .await
            .expect_err("两个出口的异常摘要都必须拒绝");
        assert!(host.applied().is_empty(), "异常摘要不得安装");
        assert!(error.contains("截断"), "截断原因缺失：{error}");
        assert!(error.contains("工具调用"), "工具调用原因缺失：{error}");
    }

    /// 截断触发备用出口重试：只安装备用出口的完整摘要，且先归档再取 provider。
    #[tokio::test]
    async fn truncated_primary_retries_fallback_and_installs_only_fallback() {
        let mut truncated = canned("【当前目标】残缺摘要");
        truncated.stop_reason = Some("length".into());
        let primary = Arc::new(CannedProvider::new(truncated));
        let primary_calls = Arc::clone(&primary.calls);
        let fallback = Arc::new(CannedProvider::new(canned("【当前目标】完整摘要")));
        let fallback_calls = Arc::clone(&fallback.calls);
        let host = FakeHost::new(
            preview_of(vec![user("继续排查")]),
            Ok("archives/x-precompact.json".into()),
            primary,
            Some(fallback),
        );

        let message = compact_history(&host, 10)
            .await
            .expect("备用出口成功应完成压缩");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            1,
            "截断必须触发备用出口重试"
        );
        let applied = host.applied();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].len(), 1);
        assert_eq!(
            applied[0][0].summary, "【当前目标】完整摘要",
            "只能安装备用出口的完整摘要"
        );
        assert!(!message.contains("残缺摘要"));
        assert!(message.contains("LLM 摘要 1 组"), "{message}");
        let steps = host.steps();
        let archive_at = steps.iter().position(|step| *step == "archive").unwrap();
        let provider_at = steps.iter().position(|step| *step == "provider").unwrap();
        assert!(
            archive_at < provider_at,
            "必须先归档成功再获取 provider：{steps:?}"
        );
    }
}
