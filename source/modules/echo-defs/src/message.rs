//! Chat-completion vocabulary: messages, tool calls, chunks, usage.

use serde::{Deserialize, Serialize};

use crate::tool::ToolDefinition;

/// Role of a message in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A single message in the conversation sent to the LLM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    /// Provider reasoning that must be returned verbatim during tool loops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Required for `role == Tool`: the id of the tool call being answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Multimodal media: image URLs or `data:` URIs attached to the message.
    ///
    /// When non-empty, providers serialize `content` as a content-part array
    /// instead of a plain string (OpenAI `image_url`, Anthropic `image` blocks).
    /// Keep the plain-text `content` so text-only pipelines are unaffected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
        }
    }

    /// Build a user message with attached images (URLs or data URIs).
    pub fn user_with_images(content: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
        }
    }
    pub fn assistant_with_reasoning(
        content: impl Into<String>,
        reasoning_content: Option<String>,
    ) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            reasoning_content,
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
        }
    }
    /// Build the assistant message that carries tool calls (content + reasoning).
    ///
    /// 规范构造器（2026-10 巡检）：内置循环与 agent-module-loop 曾各写一份等价
    /// 构造（存在漂移风险）——统一在此。
    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            reasoning_content,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            images: vec![],
        }
    }

    pub fn tool(content: impl Into<String>, tool_call_id: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Tool,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            images: vec![],
        }
    }

    /// Build a tool result message carrying attached images (URLs/data URIs).
    pub fn tool_with_images(
        content: impl Into<String>,
        tool_call_id: impl Into<String>,
        images: Vec<String>,
    ) -> Self {
        Self {
            role: ChatRole::Tool,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            images,
        }
    }
}

/// A tool call requested by the LLM.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments.
    pub arguments: String,
}

/// Incremental chunk of a tool call during streaming.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// Streaming chunk from the LLM.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatChunk {
    pub content_delta: Option<String>,
    pub reasoning_delta: Option<String>,
    pub tool_call_delta: Option<ToolCallDelta>,
}

/// Token usage reported by the provider.
///
/// `prompt_tokens` / `completion_tokens` 是 provider 直接回报的口径
///（DeepSeek：`prompt_tokens` **包含**缓存命中部分；Anthropic 官方口径
/// `input_tokens` 与 cache_read/cache_creation 分列，DeepSeek /anthropic
/// 兼容端点按 DeepSeek 文档返回 `prompt_cache_hit_tokens` /
/// `prompt_cache_miss_tokens`，命中包含在 prompt 内）。
/// `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` 在 provider 有
/// 返回时携带；`prompt_cache_creation_tokens` 保留给 Anthropic 写缓存
/// 计量（DeepSeek 无独立缓存写入价）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_hit_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_miss_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_creation_tokens: Option<u32>,
}

/// 流式增量 → 完整响应的组装器（provider 共用）。
///
/// provider 在转发每个 [`ChatChunk`] 给消费方的同时 `push` 进本组装器，
/// 流结束时 `finish()` 得到与 [`LlmProvider::chat`] 同型的 [`ChatResponse`]
/// ——流式与非流式对调用方透明（usage / stop_reason / tool_calls 由本器
/// 在流内累积）。
///
/// [`LlmProvider::chat`]: crate::llm::LlmProvider::chat
#[derive(Debug, Clone, Default)]
pub struct ChatStreamAccumulator {
    content: String,
    reasoning: String,
    /// 工具调用按 provider 的增量 `index` 归并（OpenAI `tool_calls[].index`
    /// 与 Anthropic 内容块 index 同语义）：id/name 以首个非空为准（重复
    /// 发送同值幂等），arguments 逐段拼接。
    tool_calls: std::collections::BTreeMap<usize, ToolCall>,
    usage: Usage,
    stop_reason: Option<String>,
}

impl ChatStreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 吸收一个增量。
    pub fn push(&mut self, chunk: &ChatChunk) {
        if let Some(delta) = chunk.content_delta.as_deref() {
            self.content.push_str(delta);
        }
        if let Some(delta) = chunk.reasoning_delta.as_deref() {
            self.reasoning.push_str(delta);
        }
        if let Some(delta) = &chunk.tool_call_delta {
            let entry = self.tool_calls.entry(delta.index).or_default();
            if let Some(id) = &delta.id {
                entry.id = id.clone();
            }
            if let Some(name) = &delta.name {
                entry.name = name.clone();
            }
            if let Some(arguments) = &delta.arguments {
                entry.arguments.push_str(arguments);
            }
        }
    }

    /// 用**完整值**填充某次工具调用的 arguments。
    ///
    /// 不是所有上游都按增量投递：实测同一条 ChatGPT 订阅通道，3 路并发调用
    /// 时 `function_call_arguments.delta` **一条都不发**，参数只在
    /// `arguments.done` / `item.done` / `response.output[]` 里各给一份完整值。
    /// 只认 delta 的解析器拿到空字符串，执行层再当 JSON 解析就报 EOF——
    /// 表现为"工具参数不是合法 JSON"，而模型其实给得完全正确。
    ///
    /// 判据必须幂等，否则 delta 与 done 同时到达会拼成 `{…}{…}`：
    ///
    /// - 当前为空 → 填入；
    /// - 当前是新值的**严格前缀** → 覆盖（增量只到一半）；
    /// - 与新值完全相同 → 不动（两种口径给同一份，是常态）；
    /// - 其余（当前更长 / 内容互相矛盾）→ 保留当前并返回 `false`。
    ///
    /// 返回 `false` 时调用方应记一条 warn：这说明上游两种投递口径打架，
    /// 静默取一边会让排查时看不到上游行为的漂移。
    pub fn fill_tool_arguments(&mut self, index: usize, arguments: &str) -> bool {
        let entry = self.tool_calls.entry(index).or_default();
        if entry.arguments == arguments {
            return true;
        }
        // 空前缀对任何字符串都成立，因此"当前为空"天然走覆盖分支。
        if arguments.starts_with(entry.arguments.as_str()) {
            entry.arguments.clear();
            entry.arguments.push_str(arguments);
            return true;
        }
        false
    }

    /// 用完整正文回填（`response.output_text.done` 形态），判据同上。
    ///
    /// 与工具参数的区别：正文增量是逐段推给界面的。这里**只**修 accumulator，
    /// 不补发已显示的增量——补发会重复显示。作用因此止于"持久化的事件与
    /// 下一轮上下文里的正文是完整的"。
    pub fn fill_content(&mut self, text: &str) -> bool {
        if self.content == text {
            return true;
        }
        if text.starts_with(self.content.as_str()) {
            self.content.clear();
            self.content.push_str(text);
            return true;
        }
        false
    }

    /// 用完整推理摘要回填（`response.reasoning_summary_text.done`），判据同上。
    pub fn fill_reasoning(&mut self, text: &str) -> bool {
        if self.reasoning == text {
            return true;
        }
        if text.starts_with(self.reasoning.as_str()) {
            self.reasoning.clear();
            self.reasoning.push_str(text);
            return true;
        }
        false
    }

    /// 记录 provider 上报的 usage（可多次调用，后者覆盖前者）。
    pub fn set_usage(&mut self, usage: Usage) {
        self.usage = usage;
    }

    /// 记录 provider 上报的停止原因（如 `end_turn` / `max_tokens`）。
    pub fn set_stop_reason(&mut self, reason: impl Into<String>) {
        self.stop_reason = Some(reason.into());
    }

    /// 产出完整响应；空字段归一为 `None` / 空 vec。
    pub fn finish(self) -> ChatResponse {
        ChatResponse {
            content: (!self.content.is_empty()).then_some(self.content),
            reasoning_content: (!self.reasoning.is_empty()).then_some(self.reasoning),
            tool_calls: self.tool_calls.into_values().collect(),
            usage: self.usage,
            stop_reason: self.stop_reason,
        }
    }
}

/// Default completion budget when a request doesn't pin `max_tokens`.
///
/// Anthropic 兼容端点（含 DeepSeek `/anthropic`）要求 max_tokens 必填，线上
/// 无法真正省略；这里取一个实用意义上的"无上限"值：128K 远超单轮正常输出
/// （DeepSeek V4 系列输出上限 384K），同时满足
/// `max_tokens ≤ context_window − input_tokens` 的不等式约束（1M 窗口下
/// 输入即便吃到 800K 也不会触发 400）。4096 的历史默认会在思考模式
/// （thinking 共享 completion 预算）下把可见输出烧光，表现为"空回复截断"。
pub const DEFAULT_MAX_TOKENS: u32 = 131_072;

/// A complete (non-streaming) chat response.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    /// Provider-reported stop/finish reason（Anthropic `stop_reason` 如
    /// "end_turn"/"max_tokens"；OpenAI `finish_reason` 如 "stop"/"length"）。
    /// `None` = provider 未上报（旧 mock / 不支持的端点）。
    pub stop_reason: Option<String>,
}

impl ChatResponse {
    /// 空响应（测试桩 / 流式无增量场景）。
    pub fn empty() -> Self {
        Self {
            content: None,
            reasoning_content: None,
            tool_calls: Vec::new(),
            usage: Usage::default(),
            stop_reason: None,
        }
    }

    /// 输出是否被 token 上限截断（Anthropic "max_tokens" / OpenAI "length"）。
    /// 截断时 content/tool_calls 可能只是残片，调用方应续跑而非收工。
    pub fn truncated(&self) -> bool {
        matches!(
            self.stop_reason.as_deref(),
            Some("max_tokens") | Some("length")
        )
    }
}

/// A chat completion request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[cfg(test)]
mod stream_accumulator_tests {
    use super::*;

    fn chunk(content: Option<&str>, reasoning: Option<&str>) -> ChatChunk {
        ChatChunk {
            content_delta: content.map(str::to_string),
            reasoning_delta: reasoning.map(str::to_string),
            tool_call_delta: None,
        }
    }

    /// 正文/推理逐段拼接；空流产出空响应字段。
    #[test]
    fn accumulates_content_and_reasoning() {
        let mut acc = ChatStreamAccumulator::new();
        acc.push(&chunk(Some("你"), None));
        acc.push(&chunk(None, Some("想")));
        acc.push(&chunk(Some("好"), Some("想")));
        let out = acc.finish();
        assert_eq!(out.content.as_deref(), Some("你好"));
        assert_eq!(out.reasoning_content.as_deref(), Some("想想"));
        assert!(out.tool_calls.is_empty());
        assert!(!out.truncated());
    }

    /// 工具调用按 index 归并：id/name 首个非空生效，arguments 逐段拼接；
    /// 多 index 按 index 升序输出（并行调用不乱序）。
    #[test]
    fn merges_tool_calls_by_index() {
        let mut acc = ChatStreamAccumulator::new();
        let mut push_tool =
            |index: usize, id: Option<&str>, name: Option<&str>, args: Option<&str>| {
                acc.push(&ChatChunk {
                    content_delta: None,
                    reasoning_delta: None,
                    tool_call_delta: Some(ToolCallDelta {
                        index,
                        id: id.map(str::to_string),
                        name: name.map(str::to_string),
                        arguments: args.map(str::to_string),
                    }),
                });
            };
        push_tool(1, Some("c2"), Some("read"), None);
        push_tool(0, Some("c1"), Some("bash"), Some("{\"a\""));
        push_tool(1, None, None, Some("{\"b\""));
        push_tool(0, None, None, Some(":1}"));
        push_tool(1, None, None, Some(":2}"));
        let out = acc.finish();
        assert_eq!(out.tool_calls.len(), 2);
        assert_eq!(out.tool_calls[0].id, "c1");
        assert_eq!(out.tool_calls[0].name, "bash");
        assert_eq!(out.tool_calls[0].arguments, "{\"a\":1}");
        assert_eq!(out.tool_calls[1].id, "c2");
        assert_eq!(out.tool_calls[1].arguments, "{\"b\":2}");
    }

    /// usage / stop_reason 注入：后者覆盖前者；截断语义随 stop_reason 透传。
    #[test]
    fn usage_and_stop_reason_flow_through() {
        let mut acc = ChatStreamAccumulator::new();
        acc.set_usage(Usage {
            prompt_tokens: 10,
            completion_tokens: 2,
            prompt_cache_hit_tokens: None,
            prompt_cache_miss_tokens: None,
            prompt_cache_creation_tokens: None,
        });
        acc.set_usage(Usage {
            prompt_tokens: 10,
            completion_tokens: 7,
            prompt_cache_hit_tokens: None,
            prompt_cache_miss_tokens: None,
            prompt_cache_creation_tokens: None,
        });
        acc.set_stop_reason("length");
        let out = acc.finish();
        assert_eq!(out.usage.prompt_tokens, 10);
        assert_eq!(out.usage.completion_tokens, 7);
        assert!(out.truncated());
    }

    /// 少了 delta、只来完整值 → 必须填得进去。
    ///
    /// 这是 2026-11 线上故障的最小复现：ChatGPT 订阅通道在 3 路并发调用时
    /// 一条 `function_call_arguments.delta` 都不发，参数只在 `*.done` /
    /// `item.done` / `response.output[]` 里。空累积器 + 只填完整值，得到空串，
    /// 执行层的 JSON 解析就报 EOF。
    #[test]
    fn fill_arguments_accepts_a_complete_value_without_deltas() {
        let mut acc = ChatStreamAccumulator::new();
        assert!(acc.fill_tool_arguments(0, r#"{"path":"/tmp"}"#));
        assert!(acc.fill_tool_arguments(1, r#"{"path":"/var"}"#));
        let out = acc.finish();
        assert_eq!(out.tool_calls.len(), 2);
        assert_eq!(out.tool_calls[0].arguments, r#"{"path":"/tmp"}"#);
        assert_eq!(out.tool_calls[1].arguments, r#"{"path":"/var"}"#);
    }

    /// delta 与 done 都给同一份 → 不能拼成 `{…}{…}`。
    #[test]
    fn fill_arguments_is_idempotent_beside_deltas() {
        let mut acc = ChatStreamAccumulator::new();
        acc.push(&ChatChunk {
            content_delta: None,
            reasoning_delta: None,
            tool_call_delta: Some(ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("read_file".into()),
                arguments: Some(r#"{"path":"a.rs"}"#.into()),
            }),
        });
        // 完整值先到（item.done），再重复一次（completed.output）——都不该叠加。
        assert!(acc.fill_tool_arguments(0, r#"{"path":"a.rs"}"#));
        assert!(acc.fill_tool_arguments(0, r#"{"path":"a.rs"}"#));
        let out = acc.finish();
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
    }

    /// delta 只到一半、完整值到了 → 用完整值覆盖（不是追加）。
    #[test]
    fn fill_arguments_completes_a_truncated_prefix() {
        let mut acc = ChatStreamAccumulator::new();
        acc.push(&ChatChunk {
            content_delta: None,
            reasoning_delta: None,
            tool_call_delta: Some(ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: Some(r#"{"path":"#.into()),
            }),
        });
        assert!(acc.fill_tool_arguments(0, r#"{"path":"a.rs"}"#));
        let out = acc.finish();
        assert_eq!(out.tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
    }

    /// 两种口径互相矛盾（当前更长 / 内容不同）→ 保留当前并报 false，
    /// 让调用方记 warn。静默选一边等于把上游的行为漂移藏起来。
    #[test]
    fn fill_arguments_refuses_contradictory_values() {
        let mut acc = ChatStreamAccumulator::new();
        acc.push(&ChatChunk {
            content_delta: None,
            reasoning_delta: None,
            tool_call_delta: Some(ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: Some(r#"{"path":"a.rs"}"#.into()),
            }),
        });
        assert!(
            !acc.fill_tool_arguments(0, r#"{"path":"#),
            "短值不该覆盖长值"
        );
        assert!(
            !acc.fill_tool_arguments(0, r#"{"other":1}"#),
            "同样长度但内容不同，也不该覆盖"
        );
        let out = acc.finish();
        assert_eq!(out.tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
    }

    /// 正文/推理的同型回填（`output_text.done` / `reasoning_summary_text.done`）。
    #[test]
    fn fill_content_and_reasoning_follow_the_same_rule() {
        let mut acc = ChatStreamAccumulator::new();
        acc.push(&chunk(Some("你"), Some("想")));
        // 完整值是已累积内容的前缀延伸 → 覆盖成完整值。
        assert!(acc.fill_content("你好"));
        assert!(acc.fill_reasoning("想一想"));
        // 重复（相等）与矛盾（当前不是新值前缀）都不改状态。
        assert!(acc.fill_content("你好"));
        assert!(!acc.fill_content("别的"));
        let out = acc.finish();
        assert_eq!(out.content.as_deref(), Some("你好"));
        assert_eq!(out.reasoning_content.as_deref(), Some("想一想"));
    }
}
