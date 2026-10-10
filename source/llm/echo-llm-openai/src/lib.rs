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
use echo_defs::message::{
    ChatChunk, ChatRequest, ChatResponse, ChatStreamAccumulator, ToolCall, ToolCallDelta, Usage,
};

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
    thinking: Option<echo_defs::ThinkingMode>,
    reasoning_effort: echo_defs::ReasoningEffort,
}

/// 读空闲超时（2026-10 巡检 P1）：上游连接建立后慢速滴流（每秒 1 字节）
/// 可以挂住 turn 数小时——connect_timeout 管不到已建立的连接。只杀
/// "无字节流动"：send 等首字节 / text 读体各自的空闲窗口；总时长仍
/// 不设限（合法长响应不受误伤）。
const READ_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// 包装一个 await：超过 READ_IDLE_TIMEOUT 无进展判为空闲超时。
async fn with_idle_timeout<F, T>(future: F) -> Result<T, LlmError>
where
    F: std::future::Future<Output = Result<T, reqwest::Error>>,
{
    match tokio::time::timeout(READ_IDLE_TIMEOUT, future).await {
        Ok(result) => result.map_err(|e| LlmError::Http(e.to_string())),
        Err(_) => Err(LlmError::Http(format!(
            "read idle timeout: no bytes from upstream for {}s",
            READ_IDLE_TIMEOUT.as_secs()
        ))),
    }
}

impl OpenAiProvider {
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            thinking: None,
            reasoning_effort: echo_defs::ReasoningEffort::default(),
            // 保留连接超时防止上游挂死；不设总超时——思考模式 + 大
            // completion 预算下单次响应可能合法地跑数分钟。
            client: match reqwest::Client::builder()
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
        let req = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body);
        let resp = with_idle_timeout(req.send()).await?;
        let status = resp.status();
        let text = with_idle_timeout(resp.text()).await?;
        if !status.is_success() {
            return Err(LlmError::Api(format!(
                "HTTP {status}: {}{}",
                echo_defs::token::truncate(&text, 300),
                html_endpoint_hint(&text)
            )));
        }
        let parsed: OpenAIResponse = serde_json::from_str(&text).map_err(|e| {
            LlmError::Parse(format!(
                "{e} — body: {}{}",
                echo_defs::token::truncate(&text, 300),
                html_endpoint_hint(&text)
            ))
        })?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::Api("响应中没有 choices".into()))?;
        let finish_reason = choice.finish_reason;
        let msg = choice.message;
        Ok(ChatResponse {
            content: msg.content,
            reasoning_content: msg.reasoning_content,
            stop_reason: finish_reason,
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
    ) -> Result<ChatResponse, LlmError> {
        let body =
            build_request_body_with_reasoning(request, true, self.thinking, self.reasoning_effort);
        let endpoint = format!("{}/chat/completions", self.base_url);
        let send = |body: &serde_json::Value| {
            self.client
                .post(&endpoint)
                .bearer_auth(&self.api_key)
                .json(body)
        };
        let mut resp = with_idle_timeout(send(&body).send()).await?;
        // 兼容不接受 `stream_options` 的网关：400 且原因指向该字段时去掉重试
        // 一次（OpenAI/DeepSeek/Kimi 官方端点均支持；此为第三方代理保险）。
        if resp.status().as_u16() == 400 {
            let text = resp
                .text()
                .await
                .map_err(|e| LlmError::Http(e.to_string()))?;
            if text.contains("stream_options") {
                let mut retry = body.clone();
                if let Some(obj) = retry.as_object_mut() {
                    obj.remove("stream_options");
                }
                resp = with_idle_timeout(send(&retry).send()).await?;
            } else {
                return Err(LlmError::Api(format!(
                    "HTTP 400: {}",
                    echo_defs::token::truncate(&text, 300)
                )));
            }
        }
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
        let mut acc = ChatStreamAccumulator::new();
        'outer: while let Some(chunk) = stream.next().await {
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
                        SseOutcome::Done => break 'outer,
                        SseOutcome::Chunk(chunk) => {
                            acc.push(&chunk);
                            let _ = tx.send(chunk);
                        }
                        SseOutcome::Usage(usage) => acc.set_usage(usage),
                        SseOutcome::StopReason(reason) => acc.set_stop_reason(reason),
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
        Ok(acc.finish())
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
    /// 末帧 usage（`stream_options.include_usage=true` 时携带）。
    Usage(Usage),
    /// 该 choice 的 finish_reason（"stop"/"length"/"tool_calls"，末块携带）。
    StopReason(String),
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
        // usage 帧（choices 为空）先于 choices 处理
        if let Some(usage) = parsed.usage {
            outcomes.push(SseOutcome::Usage(Usage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
            }));
        }
        let Some(choice) = parsed.choices.first() else {
            continue;
        };
        if let Some(reason) = &choice.finish_reason {
            outcomes.push(SseOutcome::StopReason(reason.clone()));
        }
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
    if stream {
        // 流式 usage：OpenAI/DeepSeek 兼容端点经 `stream_options.include_usage`
        // 在末帧回报 token 用量；不带则流式路径 usage 恒零（计量/面板归零）。
        body["stream_options"] = json!({"include_usage": true});
    }
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
    // 多模态约定：图片只走 image_url part，文本里不留 base64（否则按文本
    // token 计费）。projection 已压过一次，这里是同一规则的第二道闸门。
    let text = echo_defs::media::compact_embedded_media(&m.content, &m.images);
    let mut parts: Vec<serde_json::Value> = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }
    for image in &m.images {
        if image.starts_with("data:") {
            // 内嵌的 data: URI 原样下发（OpenAI image_url 支持 data URI）。
            parts.push(json!({
                "type": "image_url",
                "image_url": { "url": image }
            }));
        } else {
            // 远程 URL（QQ CDN 的 rkey 等会过期）不下发：端点下载失败会
            // 400 且毒化整段历史。图片在入库时已内嵌，走到这里的基本都
            // 是内嵌修复前的遗留链接，替换为占位文本。
            parts.push(json!({
                "type": "text",
                "text": "[图片链接已过期或不可用]"
            }));
        }
    }
    if parts.is_empty() {
        json!(m.content)
    } else {
        json!(parts)
    }
}

/// 当响应体是 HTML（典型：base_url 指向了网页根域而非 API 前缀，如缺少
/// `/v1`）时给出诊断提示，附在解析/HTTP 错误消息尾部。
fn html_endpoint_hint(body: &str) -> &'static str {
    let head = body.trim_start();
    let lower = head.get(..16).unwrap_or(head).to_ascii_lowercase();
    if lower.starts_with("<!doctype") || lower.starts_with("<html") {
        "\n提示：该地址返回网页而非 JSON——请检查 Base URL 是否为 API 前缀（OpenAI 兼容端点通常以 /v1 结尾）。"
    } else {
        ""
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
    /// "stop" / "length" / "tool_calls" / "content_filter"；
    /// 截断检测的唯一信号，必须透传，不能丢弃。
    finish_reason: Option<String>,
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
    /// `stream_options.include_usage` 时末帧携带（choices 为空）。
    #[serde(default)]
    usage: Option<UsageWire>,
}

#[derive(Deserialize)]
struct StreamErrorWire {
    message: String,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: DeltaWire,
    /// 末块携带："stop"/"length"/"tool_calls"（截断检测信号）。
    #[serde(default)]
    finish_reason: Option<String>,
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

    /// 流式请求携带 `stream_options.include_usage`（usage 上报）；非流式不携带。
    #[test]
    fn stream_requests_include_usage_option() {
        let request = ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage::user("hi")],
            tools: None,
            temperature: None,
            max_tokens: None,
        };
        let streaming = build_request_body(&request, true);
        assert_eq!(streaming["stream_options"]["include_usage"], true);
        let plain = build_request_body(&request, false);
        assert!(plain.get("stream_options").is_none());
    }

    /// 端到端组装：解析真实形态的 SSE 序列（正文/推理/工具调用/usage/
    /// finish_reason/[DONE]）→ 累加器产出完整响应。
    #[test]
    fn stream_sequence_assembles_full_response() {
        use echo_defs::message::{ChatStreamAccumulator, ToolCallDelta};
        let events = [
            r#"data: {"choices":[{"delta":{"reasoning_content":"让我想想"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"content":"你"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"content":"好"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"bash","arguments":"{\"cmd\""}}]},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"ls\"}"}}]},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"data: {"choices":[],"usage":{"prompt_tokens":30,"completion_tokens":12}}"#,
            "data: [DONE]",
        ];
        let mut acc = ChatStreamAccumulator::new();
        let mut done = false;
        for event in events {
            for outcome in parse_sse_event(event) {
                match outcome {
                    SseOutcome::Done => done = true,
                    SseOutcome::Chunk(chunk) => acc.push(&chunk),
                    SseOutcome::Usage(usage) => acc.set_usage(usage),
                    SseOutcome::StopReason(reason) => acc.set_stop_reason(reason),
                    SseOutcome::ApiError(msg) => panic!("unexpected error: {msg}"),
                }
            }
        }
        assert!(done, "stream must terminate with [DONE]");
        let response = acc.finish();
        assert_eq!(response.content.as_deref(), Some("你好"));
        assert_eq!(response.reasoning_content.as_deref(), Some("让我想想"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "c1");
        assert_eq!(response.tool_calls[0].name, "bash");
        assert_eq!(response.tool_calls[0].arguments, r#"{"cmd":"ls"}"#);
        assert_eq!(response.usage.prompt_tokens, 30);
        assert_eq!(response.usage.completion_tokens, 12);
        assert_eq!(response.stop_reason.as_deref(), Some("tool_calls"));
        assert!(!response.truncated());
        // 增量自身仍逐条可见（转发语义不受组装影响）
        let forwards = parse_sse_event(
            r#"data: {"choices":[{"delta":{"content":"片段"},"finish_reason":null}]}"#,
        );
        assert!(
            matches!(&forwards[0], SseOutcome::Chunk(c) if c.content_delta.as_deref() == Some("片段"))
        );
        let _ = ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments: None,
        };
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
                vec!["data:image/png;base64,QUJD".into()],
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
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn remote_image_url_becomes_placeholder_text() {
        // 远程 URL（QQ CDN rkey 会过期）不下发 image_url——端点下载失败
        // 会 400 并毒化整段历史；替换为占位文本保请求可用。
        let request = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![ChatMessage::user_with_images(
                "看图",
                vec!["https://multimedia.nt.qq.com.cn/download?rkey=expired".into()],
            )],
            tools: None,
            temperature: None,
            max_tokens: None,
        };
        let body = build_request_body(&request, false);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert!(
            !content.iter().any(|p| p["type"] == "image_url"),
            "no image_url parts for remote URLs: {content:?}"
        );
        assert!(
            content
                .iter()
                .any(|p| p["type"] == "text" && p["text"].as_str().unwrap().contains("图片")),
            "placeholder text present: {content:?}"
        );
    }

    #[test]
    fn multimodal_tool_result_carries_images() {
        let request = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![ChatMessage::tool_with_images(
                "result",
                "c1",
                vec!["data:image/png;base64,AAAA".into()],
            )],
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
