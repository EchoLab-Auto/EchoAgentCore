//! OpenAI-compatible provider.
//!
//! Works with any OpenAI-compatible endpoint (OpenAI, DeepSeek, Moonshot,
//! local vLLM, ...) by pointing `base_url` at it.

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use echo_defs::llm::LlmError;
use echo_defs::llm::LlmProvider;
use echo_defs::message::{ChatChunk, ChatRequest, ChatResponse, ToolCall, ToolCallDelta, Usage};

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
    thinking: Option<echo_defs::ThinkingMode>,
    reasoning_effort: echo_defs::ReasoningEffort,
}

impl OpenAiProvider {
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            thinking: None,
            reasoning_effort: echo_defs::ReasoningEffort::default(),
            // 超时保护：上游挂起时不能让 agent 任务无限阻塞
            client: match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
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
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn default_model(&self) -> &str {
        &self.model
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let body =
            build_request_body_with_reasoning(request, false, self.thinking, self.reasoning_effort);
        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
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
                "HTTP {status}: {}",
                echo_defs::token::truncate(&text, 300)
            )));
        }
        let parsed: OpenAIResponse = serde_json::from_str(&text).map_err(|e| {
            LlmError::Parse(format!(
                "{e} — body: {}",
                echo_defs::token::truncate(&text, 300)
            ))
        })?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::Api("响应中没有 choices".into()))?;
        let msg = choice.message;
        Ok(ChatResponse {
            content: msg.content,
            reasoning_content: msg.reasoning_content,
            tool_calls: msg
                .tool_calls
                .unwrap_or_default()
                .into_iter()
                .map(|tc| ToolCall {
                    id: tc.id,
                    name: tc.function.name,
                    arguments: tc.function.arguments,
                })
                .collect(),
            usage: parsed
                .usage
                .map(|u| Usage {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
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
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
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
            // 规范化 CRLF 与多余空白，兼容不同实现的行结尾
            buffer = buffer.replace("\r\n", "\n");
            // SSE events are separated by blank lines; process complete lines.
            while let Some(pos) = buffer.find("\n\n") {
                let event = buffer[..pos].to_string();
                buffer.drain(..pos + 2);
                for outcome in parse_sse_event(&event) {
                    match outcome {
                        SseOutcome::Done => {
                            let _ = tx.send(ChatChunk {
                                content_delta: None,
                                reasoning_delta: None,
                                tool_call_delta: None,
                            });
                            return Ok(());
                        }
                        SseOutcome::Chunk(chunk) => {
                            let _ = tx.send(chunk);
                        }
                        SseOutcome::ApiError(msg) => {
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

/// Result of parsing one SSE event block.
#[derive(Debug, PartialEq)]
enum SseOutcome {
    /// `data: [DONE]` — stream finished.
    Done,
    /// A content or tool-call delta to forward.
    Chunk(ChatChunk),
    /// In-stream error payload (HTTP 200 + `data:{"error":...}`).
    ApiError(String),
}

/// Parse one complete SSE event block (already CRLF-normalised).
///
/// Pure function extracted from `chat_stream` so the wire parsing is
/// unit-testable. Multiple deltas inside one event are all returned.
fn parse_sse_event(event: &str) -> Vec<SseOutcome> {
    let mut outcomes = Vec::new();
    for line in event.lines() {
        let line = line.trim();
        let Some(raw) = line.strip_prefix("data:") else {
            continue;
        };
        let data = raw.trim_start();
        if data == "[DONE]" {
            outcomes.push(SseOutcome::Done);
            continue;
        }
        let parsed: StreamChunk = match serde_json::from_str(data) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(error = %e, "SSE data line skipped (partial/keep-alive)");
                continue;
            }
        };
        // 流中段错误（HTTP 200 + data:{"error":...}）不能静默成功
        if let Some(err) = &parsed.error {
            outcomes.push(SseOutcome::ApiError(err.message.clone()));
            continue;
        }
        let Some(choice) = parsed.choices.first() else {
            continue;
        };
        let delta = &choice.delta;
        // 并行工具调用：逐个转发所有 delta，而不是只取第一个
        if let Some(tcs) = &delta.tool_calls {
            for tc in tcs.iter() {
                outcomes.push(SseOutcome::Chunk(ChatChunk {
                    content_delta: None,
                    reasoning_delta: None,
                    tool_call_delta: Some(ToolCallDelta {
                        index: tc.index.unwrap_or(0),
                        id: tc.id.clone(),
                        name: tc.function.as_ref().and_then(|f| f.name.clone()),
                        arguments: tc.function.as_ref().and_then(|f| f.arguments.clone()),
                    }),
                }));
            }
        }
        if let Some(content) = &delta.content {
            outcomes.push(SseOutcome::Chunk(ChatChunk {
                content_delta: Some(content.clone()),
                reasoning_delta: None,
                tool_call_delta: None,
            }));
        }
        if let Some(reasoning) = &delta.reasoning_content {
            outcomes.push(SseOutcome::Chunk(ChatChunk {
                content_delta: None,
                reasoning_delta: Some(reasoning.clone()),
                tool_call_delta: None,
            }));
        }
    }
    outcomes
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
    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .map(|m| {
            let mut v = json!({
                "role": role_str(m.role),
                "content": serialize_content(m),
            });
            if let Some(tool_calls) = &m.tool_calls {
                v["tool_calls"] = json!(tool_calls
                    .iter()
                    .map(|tc| json!({
                        "id": tc.id,
                        "type": "function",
                        "function": { "name": tc.name, "arguments": tc.arguments },
                    }))
                    .collect::<Vec<_>>());
            }
            if let Some(reasoning) = &m.reasoning_content {
                v["reasoning_content"] = json!(reasoning);
            }
            if let Some(id) = &m.tool_call_id {
                v["tool_call_id"] = json!(id);
            }
            v
        })
        .collect();
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": stream,
    });
    if let Some(tools) = &request.tools {
        body["tools"] = json!(tools.iter().map(|t| json!({
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters.clone().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            },
        })).collect::<Vec<_>>());
    }
    if let Some(t) = request.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(mt) = request.max_tokens {
        body["max_tokens"] = json!(mt);
    }
    if let Some(mode) = thinking {
        body["thinking"] = json!({"type": mode.as_str()});
        if mode == echo_defs::ThinkingMode::Enabled {
            body["reasoning_effort"] = json!(reasoning_effort.as_str());
        }
    }
    body
}

/// Serialize a message's content field.
///
/// Plain text stays a string (backward compatible). Messages carrying
/// `images` become a content-part array with `image_url` entries — the
/// OpenAI multimodal format. Non-user roles (tool/system) keep a string
/// when no images are attached, matching the strictest endpoints.
fn serialize_content(m: &echo_defs::message::ChatMessage) -> serde_json::Value {
    if m.images.is_empty() {
        return json!(m.content);
    }
    let mut parts: Vec<serde_json::Value> = Vec::new();
    if !m.content.is_empty() {
        parts.push(json!({"type": "text", "text": m.content}));
    }
    for image in &m.images {
        parts.push(json!({
            "type": "image_url",
            "image_url": { "url": image }
        }));
    }
    if parts.is_empty() {
        json!(m.content)
    } else {
        json!(parts)
    }
}

fn role_str(role: echo_defs::message::ChatRole) -> &'static str {
    match role {
        echo_defs::message::ChatRole::System => "system",
        echo_defs::message::ChatRole::User => "user",
        echo_defs::message::ChatRole::Assistant => "assistant",
        echo_defs::message::ChatRole::Tool => "tool",
    }
}

// ---- wire types -----------------------------------------------------------

#[derive(Deserialize)]
struct OpenAIResponse {
    choices: Vec<Choice>,
    usage: Option<UsageWire>,
}

#[derive(Deserialize)]
struct Choice {
    message: MessageWire,
}

#[derive(Deserialize)]
struct MessageWire {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ToolCallWire>>,
}

#[derive(Deserialize)]
struct ToolCallWire {
    id: String,
    function: FunctionWire,
}

#[derive(Deserialize)]
struct FunctionWire {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct UsageWire {
    prompt_tokens: u32,
    completion_tokens: u32,
}

#[derive(Deserialize)]
struct StreamChunk {
    /// `#[serde(default)]` required: error-only events carry no choices,
    /// and a missing field would fail the whole parse, silently skipping
    /// the in-stream error.
    #[serde(default)]
    choices: Vec<StreamChoice>,
    /// HTTP 200 + SSE error 事件时出现。
    #[serde(default)]
    error: Option<StreamErrorWire>,
}

#[derive(Deserialize)]
struct StreamErrorWire {
    message: String,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: DeltaWire,
}

#[derive(Deserialize)]
struct DeltaWire {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<StreamToolCallWire>>,
}

#[derive(Deserialize)]
struct StreamToolCallWire {
    index: Option<usize>,
    id: Option<String>,
    function: Option<StreamFunctionWire>,
}

#[derive(Deserialize)]
struct StreamFunctionWire {
    name: Option<String>,
    arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_defs::message::ChatMessage;

    #[tokio::test]
    async fn chat_parses_response() {
        let _ = ChatMessage::user("hi");
        // payload shape is exercised through build_request_body
        let body = build_request_body(
            &ChatRequest {
                model: "test".into(),
                messages: vec![ChatMessage::user("hi")],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        assert_eq!(body["model"], "test");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn body_includes_tool_calls_and_tool_role() {
        let body = build_request_body(
            &ChatRequest {
                model: "m".into(),
                messages: vec![
                    ChatMessage {
                        role: echo_defs::message::ChatRole::Assistant,
                        content: "".into(),
                        reasoning_content: None,
                        tool_calls: Some(vec![echo_defs::message::ToolCall {
                            id: "c1".into(),
                            name: "calc".into(),
                            arguments: "{\"expr\":\"1+1\"}".into(),
                        }]),
                        tool_call_id: None,
                        images: vec![],
                    },
                    echo_defs::message::ChatMessage::tool("2", "c1"),
                ],
                tools: None,
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        let msgs = body["messages"].as_array().unwrap();
        let assistant = &msgs[0];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "calc");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            "{\"expr\":\"1+1\"}"
        );
        let tool = &msgs[1];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "c1");
        assert_eq!(tool["content"], "2");
    }

    #[test]
    fn deepseek_body_enables_max_reasoning_and_returns_it_in_tool_loop() {
        let mut assistant =
            ChatMessage::assistant_with_reasoning("", Some("需要先读取文件".into()));
        assistant.tool_calls = Some(vec![echo_defs::message::ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        }]);
        let request = ChatRequest {
            model: "deepseek-v4-flash".into(),
            messages: vec![assistant, ChatMessage::tool("ok", "c1")],
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
        assert_eq!(body["reasoning_effort"], "max");
        assert_eq!(body["messages"][0]["reasoning_content"], "需要先读取文件");
    }

    #[test]
    fn disabled_reasoning_omits_effort() {
        let request = ChatRequest {
            model: "deepseek-v4-flash".into(),
            messages: vec![ChatMessage::user("hi")],
            tools: None,
            temperature: None,
            max_tokens: None,
        };
        let body = build_request_body_with_reasoning(
            &request,
            false,
            Some(echo_defs::ThinkingMode::Disabled),
            echo_defs::ReasoningEffort::Max,
        );
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn body_serializes_tool_definitions() {
        let body = build_request_body(
            &ChatRequest {
                model: "m".into(),
                messages: vec![ChatMessage::user("hi")],
                tools: Some(vec![echo_defs::tool::ToolDefinition {
                    name: "web_search".into(),
                    description: "search the web".into(),
                    parameters: Some(serde_json::json!({"type": "object"})),
                }]),
                temperature: Some(0.5),
                max_tokens: Some(100),
            },
            true,
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["max_tokens"], 100);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "web_search");
        assert_eq!(tools[0]["function"]["description"], "search the web");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn tool_definition_without_parameters_gets_empty_object() {
        let body = build_request_body(
            &ChatRequest {
                model: "m".into(),
                messages: vec![ChatMessage::user("hi")],
                tools: Some(vec![echo_defs::tool::ToolDefinition {
                    name: "no_params".into(),
                    description: "d".into(),
                    parameters: None,
                }]),
                temperature: None,
                max_tokens: None,
            },
            false,
        );
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn all_roles_are_mapped() {
        for (role, expected) in [
            (echo_defs::message::ChatRole::System, "system"),
            (echo_defs::message::ChatRole::User, "user"),
            (echo_defs::message::ChatRole::Assistant, "assistant"),
            (echo_defs::message::ChatRole::Tool, "tool"),
        ] {
            assert_eq!(role_str(role), expected);
        }
    }

    // ── SSE stream parsing ──────────────────────────────────────────────

    #[test]
    fn sse_content_delta_is_extracted() {
        let event = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}";
        let outcomes = parse_sse_event(event);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            SseOutcome::Chunk(c) => {
                assert_eq!(c.content_delta.as_deref(), Some("你好"));
                assert!(c.tool_call_delta.is_none());
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[test]
    fn sse_reasoning_delta_is_extracted() {
        let outcomes =
            parse_sse_event(r#"data: {"choices":[{"delta":{"reasoning_content":"分析中"}}]}"#);
        match &outcomes[0] {
            SseOutcome::Chunk(chunk) => {
                assert_eq!(chunk.reasoning_delta.as_deref(), Some("分析中"));
                assert!(chunk.content_delta.is_none());
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[test]
    fn sse_multiple_tool_call_deltas_are_all_forwarded() {
        // SSE `data:` payloads are single-line — the parser processes one
        // line per event.
        let event = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"calc","arguments":"{\"expr\""}},{"index":1,"id":"c2","function":{"name":"read","arguments":"{\"path\""}}]}}]}"#;
        let outcomes = parse_sse_event(event);
        assert_eq!(outcomes.len(), 2, "one outcome per tool delta");
        match &outcomes[0] {
            SseOutcome::Chunk(c) => {
                let d = c.tool_call_delta.as_ref().unwrap();
                assert_eq!(d.index, 0);
                assert_eq!(d.id.as_deref(), Some("c1"));
                assert_eq!(d.name.as_deref(), Some("calc"));
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
        match &outcomes[1] {
            SseOutcome::Chunk(c) => {
                let d = c.tool_call_delta.as_ref().unwrap();
                assert_eq!(d.index, 1);
                assert_eq!(d.id.as_deref(), Some("c2"));
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[test]
    fn sse_done_terminates() {
        assert_eq!(parse_sse_event("data: [DONE]"), vec![SseOutcome::Done]);
    }

    #[test]
    fn sse_error_payload_is_surfaced() {
        let event = r#"data: {"error":{"message":"rate limited"}}"#;
        let outcomes = parse_sse_event(event);
        assert_eq!(outcomes, vec![SseOutcome::ApiError("rate limited".into())]);
    }

    #[test]
    fn sse_ignores_non_data_lines_and_bad_json() {
        let event = "event: message\ndata: not-json\n: comment";
        assert_eq!(parse_sse_event(event), vec![]);
    }

    #[test]
    fn sse_empty_choices_are_skipped() {
        let event = r#"data: {"choices":[]}"#;
        assert_eq!(parse_sse_event(event), vec![]);
    }

    #[test]
    fn multimodal_user_message_serializes_content_parts() {
        let request = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![ChatMessage::user_with_images(
                "看看这张图",
                vec!["https://example.com/a.png".into()],
            )],
            tools: None,
            temperature: None,
            max_tokens: None,
        };
        let body = build_request_body(&request, false);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "看看这张图");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "https://example.com/a.png");
    }

    #[test]
    fn multimodal_tool_result_carries_images() {
        let request = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![
                ChatMessage::tool_with_images("result", "c1", vec!["data:image/png;base64,AAAA".into()]),
            ],
            tools: None,
            temperature: None,
            max_tokens: None,
        };
        let body = build_request_body(&request, false);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "result");
        assert_eq!(content[1]["type"], "image_url");
    }
}
