//! Anthropic Claude provider (Messages API).

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use echo_defs::llm::LlmError;
use echo_defs::llm::LlmProvider;
use echo_defs::message::{ChatChunk, ChatRequest, ChatResponse, ToolCall, ToolCallDelta, Usage};

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
    thinking: Option<echo_defs::ThinkingMode>,
    reasoning_effort: echo_defs::ReasoningEffort,
}

impl AnthropicProvider {
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            thinking: None,
            reasoning_effort: echo_defs::ReasoningEffort::default(),
            client: match reqwest::Client::builder()
                // 不设总超时：思考模式 + 大 completion 预算下，一次响应可以
                // 合法地跑好几分钟；总超时会把长生成拦腰砍断。保留连接超时，
                // 读流中断由 reqwest/turn 取消机制兜底。
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    // Extremely unlikely, but the fallback client has NO
                    // timeout — surface it so hangs are diagnosable.
                    tracing::warn!(error = %e, "reqwest builder failed, using client without timeouts");
                    reqwest::Client::new()
                }
            },
        }
    }

    pub fn with_reasoning(
        mut self,
        thinking: echo_defs::ThinkingMode,
        effort: echo_defs::ReasoningEffort,
    ) -> Self {
        self.thinking = Some(thinking);
        self.reasoning_effort = effort;
        self
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn default_model(&self) -> &str {
        &self.model
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let body =
            build_request_body_with_reasoning(request, false, self.thinking, self.reasoning_effort);
        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            // Anthropic 官方与 DeepSeek /anthropic 端点均用 x-api-key 认证
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::Api(format!(
                "HTTP {status}: {}{}",
                echo_defs::token::truncate(&text, 300),
                html_endpoint_hint(&text)
            )));
        }
        let parsed: MessagesResponse = serde_json::from_str(&text).map_err(|e| {
            LlmError::Parse(format!(
                "{e} — body: {}{}",
                echo_defs::token::truncate(&text, 300),
                html_endpoint_hint(&text)
            ))
        })?;
        let mut content = String::new();
        let mut reasoning_content = String::new();
        let mut tool_calls = Vec::new();
        for block in parsed.content {
            match block {
                ContentBlock::Text { text } => content.push_str(&text),
                ContentBlock::Thinking { thinking } => reasoning_content.push_str(&thinking),
                ContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: input.to_string(),
                    });
                }
                // 未知块（推理模型的 thinking 等）宽容跳过
                ContentBlock::Other => {}
            }
        }
        Ok(ChatResponse {
            // 只有 tool_use 没有文本时置 None，避免助手消息带空 content 被严格端点拒绝
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            reasoning_content: if reasoning_content.is_empty() {
                None
            } else {
                Some(reasoning_content)
            },
            tool_calls,
            stop_reason: parsed.stop_reason,
            usage: parsed
                .usage
                .map(|u| Usage {
                    prompt_tokens: u.input_tokens,
                    completion_tokens: u.output_tokens,
                })
                .unwrap_or_default(),
        })
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        tx: mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<(), LlmError> {
        let body =
            build_request_body_with_reasoning(request, true, self.thinking, self.reasoning_effort);
        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp
                .text()
                .await
                .map_err(|e| LlmError::Http(e.to_string()))?;
            return Err(LlmError::Api(format!(
                "HTTP {status}: {}",
                echo_defs::token::truncate(&text, 300)
            )));
        }
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| LlmError::Http(e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find("\n\n") {
                let event = buffer[..pos].to_string();
                buffer.drain(..pos + 2);
                for outcome in parse_anthropic_event(&event) {
                    match outcome {
                        AnthropicOutcome::Chunk(chunk) => {
                            let _ = tx.send(chunk);
                        }
                        AnthropicOutcome::ApiError(msg) => {
                            return Err(LlmError::Api(format!(
                                "流式响应错误: {}",
                                echo_defs::token::truncate(&msg, 300)
                            )));
                        }
                    }
                }
            }
        }
        let _ = tx.send(ChatChunk {
            content_delta: None,
            reasoning_delta: None,
            tool_call_delta: None,
        });
        Ok(())
    }
}

/// Result of parsing one Anthropic SSE event block.
#[derive(Debug, PartialEq)]
enum AnthropicOutcome {
    /// A content or tool-call delta to forward.
    Chunk(ChatChunk),
    /// In-stream `event: error` payload.
    ApiError(String),
}

/// Parse one complete Anthropic SSE event block.
///
/// Pure function extracted from `chat_stream` so the wire parsing is
/// unit-testable. The `data:` line is the last one in the event.
fn parse_anthropic_event(event: &str) -> Vec<AnthropicOutcome> {
    let mut data = None;
    for line in event.lines() {
        let line = line.trim();
        if let Some(d) = line.strip_prefix("data: ") {
            data = Some(d.to_string());
        }
    }
    let Some(data) = data else {
        return Vec::new();
    };
    let parsed: StreamEvent = match serde_json::from_str(&data) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let mut outcomes = Vec::new();
    match parsed {
        StreamEvent::ContentBlockStart {
            index,
            content_block: Some(ContentBlockStartWire::ToolUse { id, name, .. }),
        } => {
            outcomes.push(AnthropicOutcome::Chunk(ChatChunk {
                content_delta: None,
                reasoning_delta: None,
                tool_call_delta: Some(ToolCallDelta {
                    index,
                    id: Some(id),
                    name: Some(name),
                    arguments: None,
                }),
            }));
        }
        StreamEvent::ContentBlockDelta { index, delta } => match delta {
            DeltaWire::TextDelta { text } => {
                outcomes.push(AnthropicOutcome::Chunk(ChatChunk {
                    content_delta: Some(text),
                    reasoning_delta: None,
                    tool_call_delta: None,
                }));
            }
            DeltaWire::ThinkingDelta { thinking } => {
                outcomes.push(AnthropicOutcome::Chunk(ChatChunk {
                    content_delta: None,
                    reasoning_delta: Some(thinking),
                    tool_call_delta: None,
                }));
            }
            DeltaWire::InputJsonDelta { partial_json } => {
                outcomes.push(AnthropicOutcome::Chunk(ChatChunk {
                    content_delta: None,
                    reasoning_delta: None,
                    tool_call_delta: Some(ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments: Some(partial_json),
                    }),
                }));
            }
            // thinking_delta 等未知增量跳过
            DeltaWire::Other => {}
        },
        // 流中段错误不能静默当作成功
        StreamEvent::Error => outcomes.push(AnthropicOutcome::ApiError(data)),
        _ => {}
    }
    outcomes
}

/// 当响应体是 HTML（典型：base_url 指向了网页根域而非 API 端点）时给出
/// 诊断提示，附在解析/HTTP 错误消息尾部。
fn html_endpoint_hint(body: &str) -> &'static str {
    let head = body.trim_start();
    let lower = head.get(..16).unwrap_or(head).to_ascii_lowercase();
    if lower.starts_with("<!doctype") || lower.starts_with("<html") {
        "\n提示：该地址返回网页而非 JSON——请检查 Base URL 是否正确（Anthropic 兼容端点通常以 /anthropic 结尾）。"
    } else {
        ""
    }
}

#[cfg(test)]
fn build_request_body(request: &ChatRequest, stream: bool) -> serde_json::Value {
    build_request_body_with_reasoning(request, stream, None, echo_defs::ReasoningEffort::default())
}

fn build_request_body_with_reasoning(
    request: &ChatRequest,
    stream: bool,
    thinking: Option<echo_defs::ThinkingMode>,
    reasoning_effort: echo_defs::ReasoningEffort,
) -> serde_json::Value {
    let system = request
        .messages
        .iter()
        .filter(|m| m.role == echo_defs::message::ChatRole::System)
        .map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let messages: Vec<serde_json::Value> = {
        let mut out: Vec<serde_json::Value> = Vec::new();
        let mut i = 0;
        let msgs = &request.messages;
        while i < msgs.len() {
            let m = &msgs[i];
            if m.role == echo_defs::message::ChatRole::System {
                i += 1;
                continue;
            }
            if m.role == echo_defs::message::ChatRole::Tool {
                let mut results: Vec<serde_json::Value> = Vec::new();
                while i < msgs.len() && msgs[i].role == echo_defs::message::ChatRole::Tool {
                    let tm = &msgs[i];
                    results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tm.tool_call_id.clone().unwrap_or_default(),
                        "content": content_blocks(&tm.content, &tm.images),
                    }));
                    i += 1;
                }
                // User texts directly after tool results (e.g. requests that
                // followed a failed turn) must not become second consecutive
                // user messages; fold them into the same message as text
                // blocks, which Anthropic allows alongside tool_result.
                while i < msgs.len() && msgs[i].role == echo_defs::message::ChatRole::User {
                    results.push(json!({"type": "text", "text": msgs[i].content}));
                    i += 1;
                }
                out.push(json!({"role": "user", "content": results}));
            } else {
                out.push(match m.role {
                    echo_defs::message::ChatRole::User => {
                        // Merge consecutive user texts (a failed turn leaves a
                        // user message with no reply) so the request strictly
                        // alternates roles. In-memory history keeps them
                        // separate — the concurrent-reply merge keys off each
                        // message's sequence marker.
                        let mut content = m.content.clone();
                        let mut images = m.images.clone();
                        while i + 1 < msgs.len() && msgs[i + 1].role == echo_defs::message::ChatRole::User {
                            i += 1;
                            content.push('\n');
                            content.push_str(&msgs[i].content);
                            // 被合并消息的图片也要保留，不能随文本折叠丢失。
                            images.extend(msgs[i].images.iter().cloned());
                        }
                        // 多模态：带图用户消息用 content blocks（text + image）。
                        let blocks = content_blocks(&content, &images);
                        json!({"role": "user", "content": blocks})
                    }
                    echo_defs::message::ChatRole::Assistant => {
                        // Match directly on tool_calls instead of a separate
                        // has_tools guard + unwrap, so an empty/None tool_calls
                        // can never panic here.
                        match (&m.tool_calls, &m.reasoning_content) {
                            (Some(tcs), reasoning) if !tcs.is_empty() || reasoning.is_some() => {
                                let mut blocks: Vec<serde_json::Value> = Vec::new();
                                if let Some(reasoning) = reasoning {
                                    blocks.push(json!({"type": "thinking", "thinking": reasoning}));
                                }
                                if !m.content.is_empty() {
                                    blocks.push(json!({"type": "text", "text": m.content}));
                                }
                                for tc in tcs {
                                    blocks.push(json!({
                                        "type": "tool_use",
                                        "id": tc.id,
                                        "name": tc.name,
                                        "input": serde_json::from_str::<serde_json::Value>(&tc.arguments).unwrap_or_else(|_| json!({})),
                                    }));
                                }
                                json!({
                                    "role": "assistant",
                                    "content": blocks,
                                })
                            }
                            (None, Some(reasoning)) => json!({
                                "role": "assistant",
                                "content": [
                                    {"type": "thinking", "thinking": reasoning},
                                    {"type": "text", "text": m.content}
                                ]
                            }),
                            _ => json!({"role": "assistant", "content": m.content}),
                        }
                    }
                    _ => unreachable!(),
                });
                i += 1;
            }
        }
        out
    };
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": stream,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if let Some(tools) = &request.tools {
        body["tools"] = json!(tools.iter().map(|t| json!({
            "name": t.name,
            "description": t.description,
            "input_schema": t.parameters.clone().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        })).collect::<Vec<_>>());
    }
    if let Some(t) = request.temperature {
        body["temperature"] = json!(t);
    }
    // Anthropic Messages API 要求 max_tokens 必填（DeepSeek /anthropic 同样），
    // 无法真正省略；缺省给"实用无上限"的 DEFAULT_MAX_TOKENS（128K），
    // 避免思考模式烧光小预算导致空回复截断。
    body["max_tokens"] = json!(request
        .max_tokens
        .unwrap_or(echo_defs::message::DEFAULT_MAX_TOKENS));
    if let Some(mode) = thinking {
        body["thinking"] = json!({"type": mode.as_str()});
        if mode == echo_defs::ThinkingMode::Enabled {
            body["output_config"] = json!({"effort": reasoning_effort.as_str()});
        }
    }
    body
}

/// Build Anthropic content blocks for a message part.
///
/// Text-only content stays a plain string (most compact and compatible);
/// when images are attached, return blocks `[{type:text}, {type:image}]`.
/// `data:` URIs are decoded to base64 source blocks; plain URLs are sent as
/// `url` source (Anthropic supports both on the Messages API).
fn content_blocks(text: &str, images: &[String]) -> serde_json::Value {
    if images.is_empty() {
        return json!(text);
    }
    // 多模态约定：图片只走 image 块，文本里不留 base64（否则按文本 token
    // 计费，约为 image 块的两个数量级）。projection 已压过一次，这里是同一条
    // 规则的第二道闸门（工具结果、合并消息等所有调用点共用）。
    let text = echo_defs::media::compact_embedded_media(text, images);
    let text = text.as_str();
    let mut blocks: Vec<serde_json::Value> = Vec::new();
    if !text.is_empty() {
        blocks.push(json!({"type": "text", "text": text}));
    }
    for image in images {
        if let Some((media_type, data)) = parse_data_uri(image) {
            blocks.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": media_type,
                    "data": data,
                }
            }));
        } else {
            // 远程 URL（QQ CDN 的 rkey 等会过期）不再原样下发：端点下载
            // 失败会 400 且毒化整段历史。图片在入库时已内嵌为 data: URI，
            // 走到这里的基本都是内嵌修复前的遗留链接，替换为占位文本。
            blocks.push(json!({
                "type": "text",
                "text": "[图片链接已过期或不可用]"
            }));
        }
    }
    json!(blocks)
}

/// Parse a `data:image/png;base64,....` URI into `(media_type, data)`.
fn parse_data_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.split(';').next().unwrap_or("image/png").to_string();
    Some((media_type, data.to_string()))
}

// ---- wire types -----------------------------------------------------------

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    usage: Option<UsageWire>,
    /// "end_turn" / "max_tokens" / "stop_sequence" / "tool_use"；
    /// 截断检测的唯一信号，必须透传，不能丢弃。
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// 未知内容块（如推理模型的 thinking 块），宽容跳过
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct UsageWire {
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    ContentBlockStart {
        index: usize,
        content_block: Option<ContentBlockStartWire>,
    },
    ContentBlockDelta {
        index: usize,
        delta: DeltaWire,
    },
    MessageStart,
    MessageDelta,
    ContentBlockStop,
    MessageStop,
    Ping,
    Error,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// serde 在流式解析时构造这些变体；字段有意不读取（start 事件只需
// 识别 tool_use 的 id/name，其余内容由 delta 事件补齐）。
#[allow(dead_code)]
enum ContentBlockStartWire {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DeltaWire {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    #[test]
    fn html_endpoint_hint_detects_webpage_bodies() {
        assert!(!html_endpoint_hint("{\"ok\":true}").contains("提示"));
        let html = "<!doctype html>\n<html lang=\"zh-CN\"><head>...";
        let hint = html_endpoint_hint(html);
        assert!(hint.contains("返回网页"), "hint: {hint}");
        assert!(
            hint.contains("Base URL"),
            "hint must mention Base URL: {hint}"
        );
        // 大小写变体
        assert!(html_endpoint_hint("<HTML>").contains("返回网页"));
        // 前导空白容忍
        assert!(html_endpoint_hint("  \n<!DOCTYPE html>").contains("返回网页"));
    }

    use super::*;
    use echo_defs::message::{ChatMessage, ToolCall};

    #[test]
    fn parses_response_with_thinking_blocks() {
        // DeepSeek 推理模型在 Anthropic 兼容端点返回 thinking 块，
        // 必须宽容跳过而不是让整个响应解析失败
        let raw = r#"{
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "deepseek-v4-flash",
            "content": [
                {"type": "thinking", "thinking": "用户只发了一个斜杠，应该询问需求"},
                {"type": "text", "text": "你好，请问需要什么帮助？"}
            ],
            "usage": {"input_tokens": 12, "output_tokens": 18}
        }"#;
        let parsed: super::MessagesResponse = serde_json::from_str(raw).unwrap();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tool_calls = Vec::new();
        for block in parsed.content {
            match block {
                super::ContentBlock::Text { text } => content.push_str(&text),
                super::ContentBlock::Thinking { thinking } => reasoning.push_str(&thinking),
                super::ContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push((id, name, input));
                }
                super::ContentBlock::Other => {}
            }
        }
        assert_eq!(content, "你好，请问需要什么帮助？");
        assert_eq!(reasoning, "用户只发了一个斜杠，应该询问需求");
        assert!(tool_calls.is_empty());
    }

    #[test]
    fn body_strips_system_into_top_level() {
        let body = build_request_body(
            &ChatRequest {
                model: "claude-test".into(),
                messages: vec![ChatMessage::system("你是助手"), ChatMessage::user("hi")],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        assert_eq!(body["system"], "你是助手");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn deepseek_anthropic_body_enables_max_reasoning_and_replays_thinking() {
        let mut assistant = ChatMessage::assistant_with_reasoning("", Some("先调用计算器".into()));
        assistant.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            name: "calculator".into(),
            arguments: "{}".into(),
        }]);
        let request = ChatRequest {
            model: "deepseek-v4-flash".into(),
            messages: vec![assistant, ChatMessage::tool("2", "c1")],
            tools: None,
            temperature: None,
            max_tokens: None,
        };
        let body = build_request_body_with_reasoning(
            &request,
            false,
            Some(echo_defs::ThinkingMode::Enabled),
            echo_defs::ReasoningEffort::Max,
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["output_config"]["effort"], "max");
        assert_eq!(body["messages"][0]["content"][0]["type"], "thinking");
        assert_eq!(
            body["messages"][0]["content"][0]["thinking"],
            "先调用计算器"
        );
    }

    #[test]
    fn tool_results_are_grouped_into_one_user_message() {
        let body = build_request_body(
            &ChatRequest {
                model: "claude-test".into(),
                messages: vec![
                    ChatMessage::system("sys"),
                    ChatMessage::user("calculate"),
                    ChatMessage::assistant("calling"),
                    ChatMessage {
                        role: echo_defs::message::ChatRole::Assistant,
                        content: "".into(),
                        reasoning_content: None,
                        tool_calls: Some(vec![ToolCall {
                            id: "call_1".into(),
                            name: "calculator".into(),
                            arguments: "{\"expr\":\"1+1\"}".into(),
                        }]),
                        tool_call_id: None,
                        images: vec![],
                    },
                    ChatMessage::tool("2", "call_1"),
                    ChatMessage::tool("3", "call_2"),
                ],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        let messages = body["messages"].as_array().unwrap();
        // user + assistant + assistant-with-tool_use + one grouped user message.
        assert_eq!(messages.len(), 4);
        let tool_result_msg = &messages[3];
        assert_eq!(tool_result_msg["role"], "user");
        let results = tool_result_msg["content"].as_array().unwrap();
        assert_eq!(results.len(), 2, "both tool results grouped");
        assert_eq!(results[0]["tool_use_id"], "call_1");
        assert_eq!(results[0]["content"], "2");
        assert_eq!(results[1]["tool_use_id"], "call_2");
        assert_eq!(results[1]["content"], "3");
    }

    #[test]
    fn user_text_after_tool_results_folds_into_the_same_user_message() {
        // User texts directly after tool results (e.g. requests that followed
        // a failed turn) must not become consecutive user messages, which the
        // strict-role-alternation endpoints reject.
        let mut assistant = ChatMessage::assistant_with_reasoning("checking", None);
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            arguments: "{\"command\":\"ls\"}".into(),
        }]);
        let body = build_request_body(
            &ChatRequest {
                model: "claude-test".into(),
                messages: vec![
                    ChatMessage::system("sys"),
                    assistant,
                    ChatMessage::tool("ok", "call_1"),
                    ChatMessage::user("继续"),
                    ChatMessage::user("再来一次"),
                ],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        let messages = body["messages"].as_array().unwrap();
        // assistant-with-tool_use + one user message (tool_result + texts).
        assert_eq!(messages.len(), 2);
        let merged = &messages[1];
        assert_eq!(merged["role"], "user");
        let blocks = merged["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "继续");
        assert_eq!(blocks[2]["text"], "再来一次");
    }

    #[test]
    fn consecutive_user_texts_merge_into_one_message() {
        // Failed turns leave user messages without replies; the request must
        // still alternate roles.
        let body = build_request_body(
            &ChatRequest {
                model: "claude-test".into(),
                messages: vec![
                    ChatMessage::system("sys"),
                    ChatMessage::user("第一次"),
                    ChatMessage::user("第二次"),
                    ChatMessage::assistant("收到"),
                    ChatMessage::user("第三次"),
                ],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        let messages = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "user"]);
        assert!(messages[0]["content"].as_str().unwrap().contains("第一次"));
        assert!(messages[0]["content"].as_str().unwrap().contains("第二次"));
        assert_eq!(messages[2]["content"], "第三次");
    }

    #[test]
    fn assistant_tool_use_includes_input_blocks() {
        let body = build_request_body(
            &ChatRequest {
                model: "claude-test".into(),
                messages: vec![
                    ChatMessage::system("sys"),
                    ChatMessage {
                        role: echo_defs::message::ChatRole::Assistant,
                        content: "I'll check".into(),
                        reasoning_content: None,
                        tool_calls: Some(vec![ToolCall {
                            id: "c1".into(),
                            name: "read_file".into(),
                            arguments: "{\"path\":\"a.rs\"}".into(),
                        }]),
                        tool_call_id: None,
                        images: vec![],
                    },
                ],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        let msg = &body["messages"][0];
        assert_eq!(msg["role"], "assistant");
        let blocks = msg["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "text block + tool_use block");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["name"], "read_file");
        assert_eq!(blocks[1]["input"]["path"], "a.rs");
    }

    #[test]
    fn multiple_system_messages_are_joined() {
        let body = build_request_body(
            &ChatRequest {
                model: "claude-test".into(),
                messages: vec![
                    ChatMessage::system("第一部分"),
                    ChatMessage::system("第二部分"),
                    ChatMessage::user("hi"),
                ],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        assert_eq!(body["system"], "第一部分\n第二部分");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn malformed_tool_arguments_fall_back_to_empty_object() {
        let body = build_request_body(
            &ChatRequest {
                model: "claude-test".into(),
                messages: vec![
                    ChatMessage::system("sys"),
                    ChatMessage {
                        role: echo_defs::message::ChatRole::Assistant,
                        content: "".into(),
                        reasoning_content: None,
                        tool_calls: Some(vec![ToolCall {
                            id: "c1".into(),
                            name: "t".into(),
                            arguments: "not-json".into(),
                        }]),
                        tool_call_id: None,
                        images: vec![],
                    },
                ],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["input"], serde_json::json!({}));
    }

    // ── SSE stream parsing ──────────────────────────────────────────────

    #[test]
    fn anthropic_sse_text_delta() {
        let event = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}";
        let outcomes = parse_anthropic_event(event);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            AnthropicOutcome::Chunk(c) => {
                assert_eq!(c.content_delta.as_deref(), Some("你好"));
                assert!(c.tool_call_delta.is_none());
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[test]
    fn anthropic_sse_thinking_delta() {
        let event = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"分析中\"}}";
        let outcomes = parse_anthropic_event(event);
        match &outcomes[0] {
            AnthropicOutcome::Chunk(chunk) => {
                assert_eq!(chunk.reasoning_delta.as_deref(), Some("分析中"));
                assert!(chunk.content_delta.is_none());
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[test]
    fn anthropic_sse_tool_use_start() {
        let event = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"calc\",\"input\":{}}}";
        let outcomes = parse_anthropic_event(event);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            AnthropicOutcome::Chunk(c) => {
                let d = c.tool_call_delta.as_ref().unwrap();
                assert_eq!(d.index, 1);
                assert_eq!(d.id.as_deref(), Some("tool_1"));
                assert_eq!(d.name.as_deref(), Some("calc"));
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[test]
    fn anthropic_sse_input_json_delta() {
        let event = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"expr\\\":\"}}";
        let outcomes = parse_anthropic_event(event);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            AnthropicOutcome::Chunk(c) => {
                let d = c.tool_call_delta.as_ref().unwrap();
                assert_eq!(d.index, 1);
                assert_eq!(d.arguments.as_deref(), Some("{\"expr\":"));
                assert!(c.content_delta.is_none());
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[test]
    fn anthropic_sse_error_is_surfaced() {
        let event = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}";
        let outcomes = parse_anthropic_event(event);
        assert_eq!(outcomes.len(), 1);
        assert!(
            matches!(&outcomes[0], AnthropicOutcome::ApiError(msg) if msg.contains("overloaded"))
        );
    }

    #[test]
    fn anthropic_sse_ignores_heartbeats_and_stops() {
        let ping = "event: ping\ndata: {\"type\":\"ping\"}";
        assert_eq!(parse_anthropic_event(ping), vec![]);
        let stop = "event: message_stop\ndata: {\"type\":\"message_stop\"}";
        assert_eq!(parse_anthropic_event(stop), vec![]);
    }

    #[test]
    fn anthropic_sse_ignores_bad_json_and_missing_data() {
        assert_eq!(parse_anthropic_event("event: ping\ndata: not-json"), vec![]);
        assert_eq!(parse_anthropic_event(": comment only"), vec![]);
    }

    #[test]
    fn multimodal_user_message_uses_image_blocks() {
        let body = build_request_body(
            &ChatRequest {
                model: "claude-sonnet-4".into(),
                messages: vec![ChatMessage::user_with_images(
                    "看图",
                    vec!["data:image/png;base64,QUJD".into()],
                )],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        let source = &content[1]["source"];
        assert_eq!(source["type"], "base64");
        assert_eq!(source["media_type"], "image/png");
        assert_eq!(source["data"], "QUJD");
    }

    #[test]
    fn multimodal_tool_result_embeds_image_blocks() {
        let body = build_request_body(
            &ChatRequest {
                model: "claude-sonnet-4".into(),
                messages: vec![ChatMessage::tool_with_images(
                    "ok",
                    "c1",
                    vec!["data:image/png;base64,QUJD".into()],
                )],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        // tool_result 分组进 user 消息
        let users: Vec<&serde_json::Value> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "user")
            .collect();
        let blocks = users[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_result");
        let content = blocks[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["data"], "QUJD");
    }

    #[test]
    fn remote_image_url_becomes_placeholder_text() {
        // 远程 URL（QQ CDN rkey 会过期）不再原样下发为 image/url 块——
        // 端点下载失败会 400 并毒化整段历史；替换为占位文本保请求可用。
        let body = build_request_body(
            &ChatRequest {
                model: "claude-sonnet-4".into(),
                messages: vec![ChatMessage::user_with_images(
                    "看图",
                    vec!["https://multimedia.nt.qq.com.cn/download?rkey=expired".into()],
                )],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "看图");
        assert_eq!(content[1]["type"], "text");
        assert!(
            content[1]["text"].as_str().unwrap().contains("图片"),
            "placeholder text, got: {content:?}"
        );
        // 不允许再出现任何 image/url 块
        assert!(
            !content.iter().any(|b| b["type"] == "image"),
            "no image blocks for remote URLs: {content:?}"
        );
    }

    #[test]
    fn merged_user_messages_keep_all_images() {
        // 连续 user 消息合并时，被合并消息的图片不能丢。
        let body = build_request_body(
            &ChatRequest {
                model: "claude-sonnet-4".into(),
                messages: vec![
                    ChatMessage::user_with_images(
                        "第一条",
                        vec!["data:image/png;base64,QQ==".into()],
                    ),
                    ChatMessage::user_with_images(
                        "第二条",
                        vec!["data:image/png;base64,Qg==".into()],
                    ),
                ],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        let content = body["messages"][0]["content"].as_array().unwrap();
        let images: Vec<&serde_json::Value> =
            content.iter().filter(|b| b["type"] == "image").collect();
        assert_eq!(
            images.len(),
            2,
            "both merged messages' images kept: {content:?}"
        );
        assert_eq!(images[0]["source"]["data"], "QQ==");
        assert_eq!(images[1]["source"]["data"], "Qg==");
    }
}
