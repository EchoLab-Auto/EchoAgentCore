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

use echo_defs::llm::{LlmProvider, TRUNCATION_CONTINUE_PROMPT};
use echo_defs::message::{ChatMessage, ChatRequest};
use echo_defs::token::estimate_tokens;
use echo_session::event::SessionEvent;
use echo_session::{
    CompactGroup, CompactPreview, CompactionPhase, CompactionProgress, CompactionSummary,
};

/// 一次摘要调用的转录 token 预算**上限**（被替换的前缀通常远小于它；超了就
/// 按 `TRANSCRIPT_TIERS` 逐级收紧）。
///
/// 这是上限而非定值：模型窗口已知时按 [`SummaryLimits::for_context_window`]
/// 收到半窗以内——50k 输入喂给 8k 窗口的模型会先撞 HTTP 400，比截断更难恢复。
pub const TRANSCRIPT_TOKEN_BUDGET: usize = 50_000;

/// 一次摘要的输出上限（**未配置模型窗口时**的默认值）。
///
/// **这是保险丝，不是目标字数**：提示词要的是 ≤800 字（约 800–1200 token），
/// 但思考模式与正文**共享 completion 预算**（`echo_defs::message::
/// DEFAULT_MAX_TOKENS` 的注释记着同一个坑）。取 1500 时，推理模型光思考就能
/// 吃掉整笔预算，稳定撞 `max_tokens`（`ChatResponse::truncated()` 为真）→
/// 整组判失败；`groups = 1` 时一组失败即整次压缩全失败，历史一条不动、上下文
/// 照旧爆满（2026-11 实测）。
///
/// 抬到 8000 只抬高**上限**，不产生额外成本：按实际生成长度计费，且仍远低于
/// 任何主流模型的单次输出上限（提示词已把正文压在 800 字以内，正常情况根本
/// 走不到这里）。
pub const SUMMARY_MAX_TOKENS: u32 = 8_000;

/// 输出预算的下限：窗口再小也要给模型留出写四节正文的空间。
const MIN_OUTPUT_TOKENS: u32 = 256;

/// 单组摘要的截止时间（**含续写**，不是每次调用的预算）。
///
/// 截断续写会把单组从 1 次调用变成最多 [`MAX_SUMMARY_CONTINUES`] + 1 次，
/// 所以这里约束的是**整组**：开始时就定下 deadline，续写只能花剩余时间。
/// 否则单组最坏耗时 = 180s × 3，进度条上看起来就是卡死。
pub const SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// 单组内允许的续写次数（截断后让模型接着写）。
///
/// 手段与主循环同源（残片入栈 + [`TRUNCATION_CONTINUE_PROMPT`]），但**预算
/// 独立**：主循环的 [`echo_defs::llm::MAX_TRUNCATION_CONTINUES`] 是 4，因为
/// 它面对的是开放性长输出；摘要正文被提示词限在 800 字，2 次足够覆盖"思考
/// 吃掉预算"的常见形状，再多只会在真正的死循环上白烧额度。
pub const MAX_SUMMARY_CONTINUES: usize = 2;

/// 摘要请求的两项预算。
///
/// 合成一个结构体而不是两个参数：两者必须**同时**随窗口收缩——只收输入不收
/// 输出，会得到"输入 4k + 输出 8k"的超窗请求。
#[derive(Debug, Clone, Copy)]
pub struct SummaryLimits {
    /// 转录（输入）预算，见 [`TRANSCRIPT_TOKEN_BUDGET`]。
    pub transcript_budget: usize,
    /// 单次输出上限，见 [`SUMMARY_MAX_TOKENS`]。
    pub max_tokens: u32,
}

impl SummaryLimits {
    /// 按模型上下文窗口推导预算。
    ///
    /// - 输入取**半窗**（另一半留给输出、系统提示词与 token 估算误差）；
    /// - 输出取**四分之一窗**，封顶 [`SUMMARY_MAX_TOKENS`]、兜底
    ///   `MIN_OUTPUT_TOKENS`；
    /// - 窗口未知 / 为 0 → 沿用内置默认（多数部署不配 `context_window_tokens`，
    ///   这时行为与引入窗口推导之前完全一致）。
    pub fn for_context_window(window: Option<usize>) -> Self {
        match window {
            Some(window) if window > 0 => Self {
                transcript_budget: TRANSCRIPT_TOKEN_BUDGET.min(window / 2),
                max_tokens: (window / 4)
                    .clamp(MIN_OUTPUT_TOKENS as usize, SUMMARY_MAX_TOKENS as usize)
                    as u32,
            },
            _ => Self {
                transcript_budget: TRANSCRIPT_TOKEN_BUDGET,
                max_tokens: SUMMARY_MAX_TOKENS,
            },
        }
    }
}

impl Default for SummaryLimits {
    fn default() -> Self {
        Self::for_context_window(None)
    }
}

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

/// 摘要失败**按处置分类**。
///
/// 为什么不再是一个 `String`：不同失败的正确动作是互斥的。
///
/// | 失败 | 正确动作 | 换出口有用吗 |
/// |---|---|---|
/// | 截断 | 续写（残片当 assistant 前缀） | 不一定——同族模型额度一样 |
/// | 额度/限流 | **冷却该出口**，别再打 | 有用：备用是另一个 profile |
/// | 超时 / 网络 / 解析 | 换出口重试一次 | 有用 |
/// | 空 / 工具调用 | 拒绝，别重试 | 无用（形状异常，换个出口也一样） |
///
/// 压成一个字符串时，这些信息全丢了：调用方只能一律"换个出口再试"，于是
/// 429 在每一组上重复撞、截断永远得不到补救（2026-11 实测：`groups = 1` 的
/// 一次压缩因"截断 + 备用出口 429"整体失败，历史一条不动）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryError {
    /// 输出被 token 上限截断。`partial` 是已生成的部分（可作续写前缀）。
    Truncated {
        /// 已生成的残片。
        partial: String,
    },
    /// 额度 / 限流：`resets_at` 是上游给的恢复时刻（Unix 秒，可能缺）。
    Quota {
        /// 上游 HTTP 状态码（429 限流 / 402 欠费）。
        status: u16,
        /// 上游返回的错误摘要（已截断，用于排障）。
        detail: String,
        /// 上游声明的恢复时刻（Unix 秒）。
        resets_at: Option<i64>,
    },
    /// 该出口在本次压缩中已被判定不可用，**未发起调用**。
    Skipped {
        /// 冷却原因（沿用该出口上一次的真实失败）。
        reason: Box<SummaryError>,
    },
    /// 单组总预算用尽。
    Timeout,
    /// 响应里冒出工具调用（请求根本没给工具）。
    ToolCalls {
        /// 调用个数。
        count: usize,
    },
    /// 响应为空。
    Empty,
    /// 组内没有可压缩事件（不该调用模型）。
    NoEvents,
    /// 其它 provider 失败（网络 / 解析 / 配置）。
    Provider {
        /// 原始错误文案。
        detail: String,
    },
    /// 主备两个出口都失败（保留两条原因，供排障与文案排序）。
    Both {
        /// 主出口的失败。
        primary: Box<SummaryError>,
        /// 备用出口的失败。
        fallback: Box<SummaryError>,
    },
}

/// 额度类失败的默认冷却时长（上游没给 `resets_at` 时）。
///
/// 取 60s 而不是几分钟：provider 内部已经对 429 退避重试过 2 次
/// （`TRANSIENT_RETRIES`），能走到这里说明"短退避不管用"；但也不能太长——
/// 上游的限流窗口常是滚动的一分钟。
const DEFAULT_QUOTA_COOLDOWN_SECS: u64 = 60;

/// 冷却时长的上限（防止 `resets_at` 异常值把 `Instant` 推溢出）。
const MAX_QUOTA_COOLDOWN_SECS: u64 = 86_400;

impl SummaryError {
    /// 从 `LlmError` 分类：`Api` 里的 `HTTP {status}` 是唯一可靠信号。
    ///
    /// 为什么靠字符串而不是结构化字段：`LlmError`（`echo-defs`）刻意不依赖
    /// HTTP 客户端，也**没有** status 字段——providers 一律把状态码编进
    /// `Api(String)` 的前缀，格式是 `HTTP {status}[:：]…`（半角见
    /// `openai.rs` / `anthropic.rs`，全角见 `chatgpt/auth.rs`）。改 `LlmError`
    /// 的变体会波及所有 provider 与外部下游，不值。
    pub fn from_provider_error(error: &echo_defs::llm::LlmError) -> Self {
        let detail = error.to_string();
        match extract_http_status(&detail) {
            // 402 欠费 / 429 限流：都是"这个出口暂时不该再用"。
            Some(status @ (402 | 429)) => Self::Quota {
                status,
                resets_at: extract_resets_at(&detail),
                detail,
            },
            _ => Self::Provider { detail },
        }
    }

    /// 该出口应冷却到何时；`None` = 不必冷却（换出口或直接失败即可）。
    ///
    /// 只对额度类返回时长：其它失败重试是**有意义**的（网络抖动、超时），
    /// 冷却反而会误杀。
    fn cooldown(&self) -> Option<std::time::Duration> {
        match self {
            Self::Quota { resets_at, .. } => {
                let secs = match resets_at {
                    Some(at) => (at - now_unix()).clamp(0, MAX_QUOTA_COOLDOWN_SECS as i64) as u64,
                    None => DEFAULT_QUOTA_COOLDOWN_SECS,
                };
                // 已过恢复时刻（或上游是过去的时间戳）→ 仍给最小冷却，
                // 避免"命中即过期"导致对同一组连续重打。
                Some(std::time::Duration::from_secs(secs.max(1)))
            }
            _ => None,
        }
    }

    /// 报告严重度：数字越小越"难恢复"，排在最前面。
    ///
    /// 用户最先看到的那一条应该是最需要动手的那一条——之前按组序拼接，
    /// 可修复的截断会排在不可恢复的额度失败前面，排障方向直接被带偏。
    fn severity(&self) -> u8 {
        match self {
            Self::Quota { .. } => 0,
            Self::Skipped { reason } => reason.severity(),
            Self::Provider { .. } | Self::Timeout => 1,
            Self::Truncated { .. } | Self::ToolCalls { .. } | Self::Empty | Self::NoEvents => 2,
            // 合成原因取两者中更严重的那一个（数字更小）。
            Self::Both { primary, fallback } => primary.severity().min(fallback.severity()),
        }
    }
}

impl std::fmt::Display for SummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated { partial } => write!(
                f,
                "摘要输出被截断（达到 token 上限，只有 {} 字的残片；已尝试续写 {} 次）",
                partial.chars().count(),
                MAX_SUMMARY_CONTINUES
            ),
            Self::Quota {
                status, resets_at, ..
            } => {
                let what = if *status == 402 {
                    "额度不足"
                } else {
                    "被限流"
                };
                match resets_at {
                    // 已过期时不能拼成"额度已到期恢复"——那是病句，且会误导
                    // 用户继续等。单独给一句可执行的话。
                    Some(at) if *at <= now_unix() => {
                        write!(f, "摘要请求{what}（HTTP {status}；额度已到期，可立即重试）")
                    }
                    Some(at) => write!(
                        f,
                        "摘要请求{what}（HTTP {status}；额度{}恢复）",
                        humanize_reset(*at)
                    ),
                    None => write!(f, "摘要请求{what}（HTTP {status}）"),
                }
            }
            Self::Skipped { reason } => {
                write!(f, "{reason}（本次压缩已跳过对该出口的重复尝试）")
            }
            Self::Timeout => write!(
                f,
                "摘要请求超时（单组 {} 秒预算用尽，含续写）",
                SUMMARY_TIMEOUT.as_secs()
            ),
            Self::ToolCalls { count } => {
                write!(f, "摘要响应包含 {count} 个工具调用（应为纯文本）")
            }
            Self::Empty => write!(f, "摘要输出为空"),
            Self::NoEvents => write!(f, "组内没有可压缩事件"),
            Self::Provider { detail } => write!(f, "摘要请求失败：{detail}"),
            // 两条原因都保留：`both_providers_failing_keeps_both_reasons`
            // 钉住这条形状——排障时"主为什么挂 + 备为什么也挂"缺一不可。
            Self::Both { primary, fallback } => {
                write!(f, "{primary}；备用出口重试亦失败：{fallback}")
            }
        }
    }
}

/// 当前 Unix 秒。
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 从 provider 错误文案里取 `HTTP {status}` 的状态码。
fn extract_http_status(detail: &str) -> Option<u16> {
    let start = detail.find("HTTP ")? + "HTTP ".len();
    let digits: String = detail[start..]
        .chars()
        .take(3)
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// 从 provider 错误文案里取 `"resets_at":<unix>`。
///
/// 不引入 JSON 解析：这段 body 已被 provider 截断（300–400 字符），本身就
/// 不是合法 JSON；上游把 `resets_at` 放在靠前位置，子串扫描足够。
fn extract_resets_at(detail: &str) -> Option<i64> {
    const NEEDLE: &str = "\"resets_at\"";
    let rest = detail.get(detail.find(NEEDLE)? + NEEDLE.len()..)?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// 把 Unix 秒描述成"约 N 分钟/小时后"。
///
/// 刻意**不做**绝对时间格式化：标准库没有本地时区换算，引入日期库只为一行
/// 文案不划算；而"还有多久"才是用户真正要的判断依据。
///
/// 只处理**未来**时刻；已过期的分支由调用方（`Display`）单独表述。
fn humanize_reset(resets_at: i64) -> String {
    let remaining = resets_at - now_unix();
    if remaining >= 86_400 {
        format!("约 {} 天后", remaining / 86_400)
    } else if remaining >= 3_600 {
        format!("约 {} 小时后", remaining / 3_600)
    } else {
        format!("约 {} 分钟后", (remaining / 60).max(1))
    }
}

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
    /// 模型的上下文窗口（token）；`None` = 未知 / 未配置。
    ///
    /// 摘要的输入输出预算都从它推导（[`SummaryLimits::for_context_window`]）。
    /// 不读它时用内置默认（50k 输入 / 8k 输出），在**小窗口模型上会先撞
    /// HTTP 400**——那是比截断更没救的失败：请求根本没被受理，续写与换出口
    /// 都无从谈起。
    ///
    /// 默认 `None`：嵌入式宿主不认识这份配置，保持引入窗口推导之前的行为。
    fn context_window_tokens(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<usize>> + Send + '_>> {
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
    // 预算按模型窗口收缩：50k 输入 / 8k 输出在小窗口模型上会先撞 400
    // （请求根本没被受理），比截断更难恢复。窗口未知时与旧行为完全一致。
    let limits = SummaryLimits::for_context_window(host.context_window_tokens().await);
    let (summaries, llm_count, failed_count) =
        summarize_preview_with_limits(&provider, fallback.as_ref(), &preview, limits, &reporter)
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
///
/// **按严重度排序**（额度 → 其它 → 形状异常），不是按组序：用户最先看到的
/// 那条应该是最需要动手的那条。此前按组序拼接，可修复的截断会排在不可恢复的
/// 429 前面，排障方向直接被带偏（2026-11 实测：用户先看到"摘要输出被截断"，
/// 而真正让整次压缩失败的是备用出口的 429）。
fn failure_reasons(summaries: &[SummaryOrFallback]) -> Vec<String> {
    let mut ranked: Vec<(u8, String)> = summaries
        .iter()
        .filter_map(|summary| {
            summary
                .failure_reason
                .as_ref()
                .map(|error| (error.severity(), error.to_string()))
        })
        .collect();
    ranked.sort_by_key(|(severity, _)| *severity);
    let mut out: Vec<String> = Vec::new();
    for (_, text) in ranked {
        // 完全相同的原因只留一份。`dedup` 不够：它只去**相邻**重复，而排序
        // 打乱了原本的组序。
        if !out.contains(&text) {
            out.push(text);
        }
    }
    out
}

/// 一个出口的冷却状态（本次压缩内有效）。
#[derive(Default)]
struct OutletState {
    /// 冷却截止时刻；`None` = 从未命中额度类失败。
    until: Option<tokio::time::Instant>,
    /// 上一次真实失败原因（跳过时沿用它，不丢排障信息）。
    last: Option<SummaryError>,
}

impl OutletState {
    /// 现在可否调用；不可用时给出"本次已跳过"的原因。
    fn blocked(&self) -> Option<SummaryError> {
        let until = self.until?;
        if tokio::time::Instant::now() < until {
            return Some(SummaryError::Skipped {
                reason: Box::new(self.last.clone()?),
            });
        }
        None
    }

    /// 记下一次真实失败：只有额度类会真的把出口打进冷却。
    fn record(&mut self, error: &SummaryError) {
        self.last = Some(error.clone());
        self.until = error
            .cooldown()
            .map(|duration| tokio::time::Instant::now() + duration);
    }
}

/// 本次压缩内的**出口冷却**（主 / 备各一个槽位）。
///
/// 为什么是局部状态而不是进程级单例：
///
/// - 需求本身就是"**同一次压缩里**后续组别再撞同一下不可用的出口"——"后续组"
///   只在一次压缩内存在；
/// - 进程级静态状态会被并行测试互相污染（`ScriptedProvider` 用 402 模拟失败，
///   一条用例的熔断会让其它用例全部跳过），也会让"用户手动再点一次压缩"
///   静默什么都不做；
/// - 跨压缩的冷却上游自己会声明 `resets_at`，用户的下一步动作本来就该是等它。
///
/// 注意冷却的是**槽位**不是"额度池"：主出口欠费（402）时换备用出口（另一个
/// profile）是 2026-10 修复的正解，必须继续允许。
#[derive(Default)]
struct OutletCooldown {
    primary: OutletState,
    fallback: OutletState,
}

/// 一组的执行结果：`(摘要正文, 是否拿到可安装摘要, 失败原因)`。
type GroupOutcome = (String, bool, Option<SummaryError>);

/// 单组摘要：主出口 →（必要时）备用出口，两者都受本组冷却约束。
///
/// 与旧实现的顺序差异：旧实现"主出口失败就立刻换出口"，把额度类失败也换了
/// 过去——两个出口若指向同一额度池，只会白打一次。现在先看槽位是否在冷却，
/// 在冷却就直接跳过并沿用上次原因。
async fn summarize_one(
    provider: &Arc<dyn LlmProvider>,
    fallback: Option<&Arc<dyn LlmProvider>>,
    group: &CompactGroup,
    limits: SummaryLimits,
    cooldown: &mut OutletCooldown,
) -> GroupOutcome {
    let primary = match cooldown.primary.blocked() {
        Some(skipped) => Err(skipped),
        None => match summarize_group_with(provider, group, limits).await {
            Ok(body) => Ok(body),
            Err(error) => {
                cooldown.primary.record(&error);
                Err(error)
            }
        },
    };
    match primary {
        Ok(body) => (body, true, None),
        Err(primary_error) => {
            let Some(alternative) = fallback else {
                return (String::new(), false, Some(primary_error));
            };
            // 2026-10 修复：人格 profile 欠费（HTTP 402）/ 端点不可用时，
            // 此前整组直接降级为统计文案，历史信息全丢。这里换全局默认
            // 出口重试一次；两次失败原因都留在失败原因里便于排障。
            let retry = match cooldown.fallback.blocked() {
                Some(skipped) => Err(skipped),
                None => match summarize_group_with(alternative, group, limits).await {
                    Ok(body) => Ok(body),
                    Err(error) => {
                        cooldown.fallback.record(&error);
                        Err(error)
                    }
                },
            };
            match retry {
                Ok(body) => {
                    tracing::warn!(
                        session = group.session.as_deref().unwrap_or("<legacy>"),
                        "compaction: primary provider failed, fallback provider succeeded"
                    );
                    (body, true, None)
                }
                Err(fallback_error) => (
                    String::new(),
                    false,
                    Some(SummaryError::Both {
                        primary: Box::new(primary_error),
                        fallback: Box::new(fallback_error),
                    }),
                ),
            }
        }
    }
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
///
/// 预算用内置默认（[`SummaryLimits::default`]）；生产路径走
/// [`summarize_preview_with_limits`] 以便按模型窗口收缩。
pub async fn summarize_preview_with_progress(
    provider: &Arc<dyn LlmProvider>,
    fallback: Option<&Arc<dyn LlmProvider>>,
    preview: &CompactPreview,
    _keep_recent: usize,
    progress: &(dyn Fn(usize, usize, Option<&str>) + Sync),
) -> (Vec<SummaryOrFallback>, usize, usize) {
    summarize_preview_with_limits(
        provider,
        fallback,
        preview,
        SummaryLimits::default(),
        progress,
    )
    .await
}

/// 同 [`summarize_preview_with_progress`]，但预算显式给出。
pub async fn summarize_preview_with_limits(
    provider: &Arc<dyn LlmProvider>,
    fallback: Option<&Arc<dyn LlmProvider>>,
    preview: &CompactPreview,
    limits: SummaryLimits,
    progress: &(dyn Fn(usize, usize, Option<&str>) + Sync),
) -> (Vec<SummaryOrFallback>, usize, usize) {
    let mut cooldown = OutletCooldown::default();
    let mut out = Vec::new();
    let mut llm_count = 0usize;
    let mut failed_count = 0usize;
    let total = preview.groups.len();
    for (index, group) in preview.groups.iter().enumerate() {
        let (summary, from_llm, failure_reason) =
            summarize_one(provider, fallback, group, limits, &mut cooldown).await;
        if from_llm {
            llm_count += 1;
        } else {
            failed_count += 1;
            // 失败组不生成摘要（PR1）：绝不用统计文案替换历史。
        }
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
    ///
    /// 用 [`SummaryError`] 而不是 `String`：调用方需要按**类别**排序（见
    /// `failure_reasons`），字符串只能靠关键字猜。
    pub failure_reason: Option<SummaryError>,
}

/// 用模型摘要一组（内置默认预算）。`Err` 是**分类过**的失败原因。
///
/// 只接受**完整纯文本**：非空但被 token 上限截断（`stop_reason` 为
/// `max_tokens` / `length`）、或响应里带工具调用（请求根本没给工具）都算
/// 残片，一律拒绝（PR1）。
pub async fn summarize_group(
    provider: &Arc<dyn LlmProvider>,
    group: &CompactGroup,
) -> Result<String, SummaryError> {
    summarize_group_with(provider, group, SummaryLimits::default()).await
}

/// 同 [`summarize_group`]，但预算显式给出。
///
/// 与旧实现的唯一行为差异：**截断不再一次判死**。残片会被当成 assistant
/// 前缀、追加 [`TRUNCATION_CONTINUE_PROMPT`] 让模型接着写（上限
/// [`MAX_SUMMARY_CONTINUES`] 次，与主循环同源的手段）。
///
/// 为什么值得续写：压缩的输入是一整段历史，输出是一份四节摘要——一旦在半途
/// 被 `max_tokens` 切断，换个出口再问一遍等于把整段输入重发一次，而续写只花
/// 一个残片的钱。这也是主循环早就在做的事（`runner.rs` 的截断续跑），压缩
/// 路径此前漏了。
///
/// 单组的总时限是 [`SUMMARY_TIMEOUT`]（**含**全部续写），用同一个 deadline
/// 约束，否则最坏耗时 = 180s × 3，在进度条上就是"卡住"。
pub async fn summarize_group_with(
    provider: &Arc<dyn LlmProvider>,
    group: &CompactGroup,
    limits: SummaryLimits,
) -> Result<String, SummaryError> {
    if group.replaced.is_empty() {
        return Err(SummaryError::NoEvents);
    }
    let deadline = tokio::time::Instant::now() + SUMMARY_TIMEOUT;
    let transcript = render_transcript_with_budget(&group.replaced, limits.transcript_budget);
    let mut messages = vec![
        ChatMessage::system(SUMMARY_SYSTEM_PROMPT),
        ChatMessage::user(format!("旧对话记录：\n\n{transcript}")),
    ];
    // 逐轮累积：续写出来的片段拼在残片之后，拼的是一份**连续**的摘要。
    // 这里刻意不同于主循环（它只保留最后一轮的 content）——主循环里前半截
    // 已经作为 assistant 消息留在 messages 里、可被后续轮读到；压缩这里
    // 只要最终那一份摘要文本，丢了就真丢了。
    let mut assembled = String::new();
    for round in 0..=MAX_SUMMARY_CONTINUES {
        let request = ChatRequest {
            model: provider.default_model().to_string(),
            messages: messages.clone(),
            tools: None,
            temperature: None,
            max_tokens: Some(limits.max_tokens),
        };
        let response = match tokio::time::timeout_at(deadline, provider.chat(&request)).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return Err(SummaryError::from_provider_error(&error)),
            Err(_) => return Err(SummaryError::Timeout),
        };
        if !response.tool_calls.is_empty() {
            return Err(SummaryError::ToolCalls {
                count: response.tool_calls.len(),
            });
        }
        // 先取截断判定再解构 content：`unwrap_or_default` 会移走 content。
        let truncated = response.truncated();
        let piece = response.content.unwrap_or_default();
        // 累积到同一份摘要：续写出来的片段接在残片之后（见上文的说明）。
        assembled.push_str(&piece);
        if !truncated {
            let body = assembled.trim();
            if body.is_empty() {
                return Err(SummaryError::Empty);
            }
            return Ok(body.to_string());
        }
        // 残片为空 = 模型把整笔预算花在了思考上（或直接吐出空 delta）。
        // 续写只会以同样的理由再截一次，直接报截断让人去调预算。
        if piece.trim().is_empty() {
            break;
        }
        tracing::warn!(
            round,
            partial_chars = piece.chars().count(),
            max_tokens = limits.max_tokens,
            "compaction: summary truncated at token limit; continuing"
        );
        messages.push(ChatMessage::assistant(piece));
        messages.push(ChatMessage::user(TRUNCATION_CONTINUE_PROMPT));
    }
    Err(SummaryError::Truncated {
        partial: assembled.trim().to_string(),
    })
}

/// 把被替换的事件渲染成带角色标签的转录（内置预算）。
pub fn render_transcript(events: &[SessionEvent]) -> String {
    render_transcript_with_budget(events, TRANSCRIPT_TOKEN_BUDGET)
}

/// 同 [`render_transcript`]，但预算显式给出（见 [`SummaryLimits`]）。
///
/// 逐级收缩直到装进 `budget`：先收紧单事件上限，仍超就丢最老的事件
/// （新上下文优先）。
pub fn render_transcript_with_budget(events: &[SessionEvent], budget: usize) -> String {
    for caps in TRANSCRIPT_TIERS {
        let text = render_with_caps(events, caps);
        if estimate_tokens(&text) <= budget {
            return text;
        }
    }
    // 最后手段：只留有最新的一批。丢前缀使转录单调收缩，所以二分找截断点，
    // 不必每丢一条就重渲染（O(n log n) vs O(n²)）。
    let caps = TRANSCRIPT_TIERS[TRANSCRIPT_TIERS.len() - 1];
    let fits = |start: usize| estimate_tokens(&render_with_caps(&events[start..], caps)) <= budget;
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
    if estimate_tokens(&text) > budget {
        let head: String = text.chars().take(budget / 2).collect();
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
        let reason = summaries[0]
            .failure_reason
            .as_ref()
            .map(|error| error.to_string())
            .unwrap_or_default();
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
            .as_ref()
            .map(|error| error.to_string())
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
        let error = error.to_string();
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
        let error = error.to_string();
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
        // 主出口**先续写**再放弃：`CannedProvider` 每次都返回同一个截断响应，
        // 所以首发 + 每次续写都撞一次上限，共 `MAX_SUMMARY_CONTINUES + 1` 次。
        // 旧行为是"截断即死"（1 次），但那等于把可续写的半份摘要直接扔掉。
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            MAX_SUMMARY_CONTINUES + 1,
            "截断必须先尝试续写，用完预算才换出口"
        );
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

    // ── 失败分类 / 续写 / 出口冷却（2026-11）─────────────────────────────

    /// 按调用序返回预设结果的 provider：用来表达"第一次截断、第二次正常"
    /// 这类**跨轮**形状——`CannedProvider` 每次返回同一个响应，做不到。
    struct SequenceProvider {
        script: std::sync::Mutex<
            std::collections::VecDeque<
                Result<echo_defs::message::ChatResponse, echo_defs::llm::LlmError>,
            >,
        >,
        calls: Arc<AtomicUsize>,
    }

    impl SequenceProvider {
        fn new(
            script: Vec<Result<echo_defs::message::ChatResponse, echo_defs::llm::LlmError>>,
        ) -> Self {
            Self {
                script: std::sync::Mutex::new(script.into()),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for SequenceProvider {
        fn name(&self) -> &str {
            "sequence"
        }
        fn default_model(&self) -> &str {
            "sequence-model"
        }
        async fn chat(
            &self,
            _request: &ChatRequest,
        ) -> Result<echo_defs::message::ChatResponse, echo_defs::llm::LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut queue = self.script.lock().unwrap();
            // 脚本用尽后重复最后一个结果（续写循环会多打几次）。
            if queue.len() > 1 {
                queue.pop_front().unwrap()
            } else {
                match queue.front() {
                    Some(Ok(response)) => Ok(response.clone()),
                    Some(Err(error)) => Err(clone_llm_error(error)),
                    None => Err(echo_defs::llm::LlmError::Api("脚本为空".into())),
                }
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

    /// `LlmError` 没有 `Clone`（`thiserror` 派生只给了 `Display`）。测试里用
    /// 字符串重建一个等价错误——分类只读字符串，足够。
    fn clone_llm_error(error: &echo_defs::llm::LlmError) -> echo_defs::llm::LlmError {
        echo_defs::llm::LlmError::Api(error.to_string())
    }

    fn truncated(content: &str) -> echo_defs::message::ChatResponse {
        let mut response = canned(content);
        response.stop_reason = Some("max_tokens".into());
        response
    }

    fn quota_error(status: u16) -> echo_defs::llm::LlmError {
        echo_defs::llm::LlmError::Api(format!(
            "HTTP {status}：{{\"error\":{{\"type\":\"usage_limit_reached\",\"resets_at\":{}}}}}",
            now_unix() + 7_200
        ))
    }

    /// 截断是**可续写的**：第二轮回后半截，两段拼成一份完整摘要。
    ///
    /// 旧行为是"截断即判失败"——把模型已经写出来的半份摘要直接扔掉，然后
    /// 换个出口把整段输入重发一次。这条测试钉住"续写优先"。
    #[tokio::test]
    async fn truncated_summary_is_continued_and_spliced() {
        let provider: Arc<dyn LlmProvider> = Arc::new(SequenceProvider::new(vec![
            Ok(truncated("【当前目标】排查自更新")),
            Ok(canned("模块的部署形状")),
        ]));
        let body = summarize_group_with(
            &provider,
            &group(vec![user("继续")]),
            SummaryLimits::default(),
        )
        .await
        .expect("第二轮写完即为成功");
        assert_eq!(
            body, "【当前目标】排查自更新模块的部署形状",
            "续写片段必须拼在残片之后，而不是丢掉前半截"
        );
    }

    /// 续写无果（每轮都截断）→ 报 `Truncated` 且带上已生成的残片。
    ///
    /// 残片有内容时不能再降级成"空"：那是模型确实写了东西，只是没写完。
    #[tokio::test]
    async fn exhausted_continuations_report_truncated_with_partial() {
        let provider: Arc<dyn LlmProvider> =
            Arc::new(SequenceProvider::new(vec![Ok(truncated("半截摘要"))]));
        let error = summarize_group_with(
            &provider,
            &group(vec![user("继续")]),
            SummaryLimits::default(),
        )
        .await
        .expect_err("每轮都截断必须按失败处理");
        match error {
            SummaryError::Truncated { partial } => {
                assert!(!partial.is_empty(), "残片要跟着错误一起报出来");
            }
            other => panic!("应为 Truncated，实际 {other:?}"),
        }
    }

    /// 额度失败**冷却该槽位**：同一次压缩里后续组不再打同一个出口。
    ///
    /// 主备**各自**冷却：主出口 429 不等于备用出口不可用（备用是另一个
    /// profile/额度池，这正是 2026-10 修复的场景）。所以第一组仍然会试备用，
    /// 之后两组才两个槽都跳过。
    #[tokio::test]
    async fn quota_failure_cools_both_outlets_and_skips_later_groups() {
        let primary = Arc::new(SequenceProvider::new(vec![Err(quota_error(429))]));
        let fallback = Arc::new(SequenceProvider::new(vec![Err(quota_error(429))]));
        let primary_calls = Arc::clone(&primary.calls);
        let fallback_calls = Arc::clone(&fallback.calls);
        let provider: Arc<dyn LlmProvider> = primary;
        let alternative: Arc<dyn LlmProvider> = fallback;

        let preview = CompactPreview {
            groups: vec![
                group(vec![user("a")]),
                group(vec![user("b")]),
                group(vec![user("c")]),
            ],
            total_replaced: 3,
            total_events: 3,
        };
        let (summaries, llm, failed) = summarize_preview_with_progress(
            &provider,
            Some(&alternative),
            &preview,
            1,
            &|_done, _total, _session| {},
        )
        .await;

        assert_eq!((llm, failed), (0, 3), "三组都失败");
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            1,
            "主出口 429 后必须冷却：后续组不得再打它"
        );
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            1,
            "备用出口同样冷却；但不是被主出口的 429 连坐"
        );
        assert!(
            summaries[1]
                .failure_reason
                .as_ref()
                .is_some_and(|error| error.to_string().contains("跳过")),
            "被跳过的组要如实说明原因：{:?}",
            summaries[1].failure_reason
        );
    }

    /// 额度类文案把 `resets_at` 说成"还有多久"，不把原始 JSON 抛给用户。
    #[test]
    fn quota_message_humanizes_resets_at() {
        let soon = SummaryError::Quota {
            status: 429,
            detail: "{\"resets_at\":123}".into(),
            resets_at: Some(now_unix() + 7_200),
        };
        let text = soon.to_string();
        assert!(text.contains("429"), "{text}");
        assert!(text.contains("约 2 小时后"), "要给出相对时间：{text}");
        assert!(
            !text.contains("resets_at"),
            "原始 JSON 不该出现在文案里：{text}"
        );

        let unknown = SummaryError::Quota {
            status: 402,
            detail: String::new(),
            resets_at: None,
        };
        assert!(unknown.to_string().contains("额度不足"), "{unknown}");

        // 已过期：不能拼成"额度已到期恢复"这种病句，也不能让用户继续等。
        let expired = SummaryError::Quota {
            status: 429,
            detail: String::new(),
            resets_at: Some(now_unix() - 60),
        };
        let text = expired.to_string();
        assert!(text.contains("已到期"), "{text}");
        assert!(text.contains("可立即重试"), "{text}");
        assert!(!text.contains("恢复）"), "过期不该再说等它恢复：{text}");
    }

    /// 失败原因按**可操作性**排序：不可恢复的额度失败排最前。
    ///
    /// 此前按组序拼接，用户先看到"摘要输出被截断"，而真正让压缩失败的是
    /// 后面那条 429——排障方向被带偏。
    #[test]
    fn failure_reasons_rank_quota_before_repairable_ones() {
        let summaries = vec![
            SummaryOrFallback {
                session: None,
                summary: String::new(),
                from_llm: false,
                failure_reason: Some(SummaryError::Truncated {
                    partial: "半截".into(),
                }),
            },
            SummaryOrFallback {
                session: None,
                summary: String::new(),
                from_llm: false,
                failure_reason: Some(SummaryError::Quota {
                    status: 429,
                    detail: String::new(),
                    resets_at: None,
                }),
            },
        ];
        let reasons = failure_reasons(&summaries);
        assert_eq!(reasons.len(), 2);
        assert!(
            reasons[0].contains("429"),
            "额度失败必须排第一：{reasons:?}"
        );
        assert!(reasons[1].contains("截断"), "{reasons:?}");
    }

    /// 同一原因不因跨组重复而刷屏（`dedup` 只能去相邻重复）。
    #[test]
    fn failure_reasons_deduplicate_non_adjacent() {
        let truncated = || SummaryOrFallback {
            session: None,
            summary: String::new(),
            from_llm: false,
            failure_reason: Some(SummaryError::Truncated {
                partial: "x".into(),
            }),
        };
        let quota = SummaryOrFallback {
            session: None,
            summary: String::new(),
            from_llm: false,
            failure_reason: Some(SummaryError::Quota {
                status: 429,
                detail: String::new(),
                resets_at: None,
            }),
        };
        let reasons = failure_reasons(&vec![truncated(), quota, truncated()]);
        assert_eq!(reasons.len(), 2, "同一条截断原因只报一次：{reasons:?}");
    }

    /// 预算随窗口收缩：小窗口不能拼出"输入半窗 + 输出 8k"的超窗请求。
    #[test]
    fn summary_limits_shrink_with_context_window() {
        // 未配置窗口 → 内置默认，与引入窗口推导之前一致。
        let default = SummaryLimits::for_context_window(None);
        assert_eq!(default.transcript_budget, TRANSCRIPT_TOKEN_BUDGET);
        assert_eq!(default.max_tokens, SUMMARY_MAX_TOKENS);

        // 大窗口（1M）→ 仍受内置上限约束，不会无限放大。
        let large = SummaryLimits::for_context_window(Some(1_000_000));
        assert_eq!(large.transcript_budget, TRANSCRIPT_TOKEN_BUDGET);
        assert_eq!(large.max_tokens, SUMMARY_MAX_TOKENS);

        // 8k 窗口 → 输入 4k、输出 2k，合计不超窗。
        let small = SummaryLimits::for_context_window(Some(8_192));
        assert_eq!(small.transcript_budget, 4_096);
        assert_eq!(small.max_tokens, 2_048);
        assert!(
            small.transcript_budget + small.max_tokens as usize <= 8_192,
            "输入 + 输出必须装得下窗口：{small:?}"
        );

        // 极窄窗口 → 输出兜底到下限，不会被压成 0（那样永远写不出摘要）。
        let tiny = SummaryLimits::for_context_window(Some(600));
        assert_eq!(tiny.max_tokens, MIN_OUTPUT_TOKENS);
    }

    /// 转录预算跟着 `SummaryLimits` 走（同一个窗口推导结果必须传得下去）。
    #[test]
    fn transcript_respects_explicit_budget() {
        let mut events = Vec::new();
        for index in 0..50 {
            events.push(user(&format!("第{index}条：{}", "x".repeat(2000))));
        }
        let generous = render_transcript_with_budget(&events, 50_000);
        let tight = render_transcript_with_budget(&events, 2_000);
        assert!(
            estimate_tokens(&tight) < estimate_tokens(&generous),
            "收紧预算必须真的少渲染"
        );
        assert!(tight.contains("更早记录已省略"), "丢前缀要显式标注");
    }

    /// 429 之外的重试仍然换出口（冷却只针对额度类）。
    #[tokio::test]
    async fn transient_failure_still_retries_fallback() {
        let primary = Arc::new(SequenceProvider::new(vec![Err(
            echo_defs::llm::LlmError::Http("connection reset".into()),
        )]));
        let fallback = Arc::new(SequenceProvider::new(vec![Ok(canned("【当前目标】ok"))]));
        let provider: Arc<dyn LlmProvider> = primary;
        let alternative: Arc<dyn LlmProvider> = fallback;
        let (summaries, llm, failed) = summarize_preview(
            &provider,
            Some(&alternative),
            &preview_of(vec![user("a")]),
            10,
        )
        .await;
        assert_eq!((llm, failed), (1, 0), "网络抖动必须允许换出口重试");
        assert!(summaries[0].summary.contains("ok"));
    }
}
