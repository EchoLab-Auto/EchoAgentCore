//! OpenAI **Responses API** 客户端（ChatGPT 订阅通道）。
//!
//! # 为什么必须另写一个客户端
//!
//! Core 原有的 `openai` provider 只讲 `/chat/completions`。而 ChatGPT 订阅
//! 走的是另一条线：`https://chatgpt.com/backend-api/codex/responses`。
//! 官方 Codex 二进制里 `chat/completions` 出现 **0 次**、`/responses` 出现
//! 37 次——**这条端点没有 chat-completions 形态**，所以不能靠改 base_url 复用。
//!
//! # 与 `openai.rs` 的三处结构差异
//!
//! | | chat/completions | Responses |
//! |---|---|---|
//! | 系统提示词 | `messages[0].role=system` | 顶层 `instructions`（不在 input 里） |
//! | 工具调用 | 嵌在 assistant 消息的 `tool_calls` | **独立 item** `{type:"function_call"}` |
//! | 工具结果 | `{role:"tool", tool_call_id}` | `{type:"function_call_output", call_id}` |
//!
//! # 流式
//!
//! `stream: true` 时是 SSE：每行 `event: <name>` + `data: <json>`。我们只认
//! 下面几种（其余静默跳过——新事件类型随时会加，硬失败只会让上游一升级就断）：
//!
//! - `response.output_text.delta` → 正文增量
//! - `response.reasoning_summary_text.delta` → 推理摘要增量
//! - `response.output_item.added`（`function_call`）→ 工具调用开始
//! - `response.function_call_arguments.delta` → 工具参数增量
//! - `response.completed` → usage / stop_reason
//! - `response.failed` / `error` → 明确失败
//!
//! # 非流式
//!
//! `chat()` 直接复用 `chat_stream()` 并用 `ChatStreamAccumulator` 累加：
//! 两条路径的解析逻辑因此只有一份。这不是偷懒——`/responses` 的非流式响应
//! 只是把同一批 item 一次性给出来，分别实现会有两份容易走偏的映射。

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use echo_defs::llm::LlmError;
use echo_defs::llm::LlmProvider;
use echo_defs::message::{
    ChatChunk, ChatMessage, ChatRequest, ChatResponse, ChatRole, ChatStreamAccumulator, ToolCallDelta,
};

use super::auth::{self, ChatGptAuth};

/// 读空闲超时（与 `openai.rs` 同一口径）：上游慢速滴流不该挂住整个 turn。
const READ_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// ChatGPT 订阅 provider。
pub struct ChatGptProvider {
    model: String,
    /// 登录态快照。过期时由 `fresh_auth()` 刷新并写回磁盘——所以这里必须是
    /// 可变共享：一个 provider 实例可能服务很多次请求，跨请求复用新令牌，
    /// 否则每次都要读盘 + 可能重复刷新。
    auth: tokio::sync::Mutex<ChatGptAuth>,
    client: reqwest::Client,
    reasoning_effort: echo_defs::ReasoningEffort,
}

impl ChatGptProvider {
    /// 用一份已加载的令牌建 provider。
    ///
    /// `client` 构建失败**必须**返回错误，不能退化成默认 client：默认 client
    /// 不认 `ECHO_CHATGPT_PROXY`，会变成直连——而直连在这台机器上必然失败。
    /// 于是"代理地址写错"会表现成"用户发消息时报网络错误"，排查要从网络查起，
    /// 真正的原因（一个拼错的 URL）反而藏在没人看的 warn 日志里。
    pub fn new(auth: ChatGptAuth, model: &str) -> Result<Self, LlmError> {
        // 与登录共用同一个 client 构造器：代理配置只写一处，
        // 不会出现"登录通了、调用超时"的半通状态。
        let client = auth::build_client().map_err(LlmError::Config)?;
        Ok(Self {
            model: model.to_string(),
            auth: tokio::sync::Mutex::new(auth),
            reasoning_effort: echo_defs::ReasoningEffort::default(),
            client,
        })
    }

    pub fn with_reasoning(mut self, effort: echo_defs::ReasoningEffort) -> Self {
        self.reasoning_effort = effort;
        self
    }

    /// 取一份未过期的令牌（必要时刷新并写回）。
    async fn fresh_auth(&self) -> Result<ChatGptAuth, LlmError> {
        let mut guard = self.auth.lock().await;
        if !guard.is_expired() {
            return Ok(guard.clone());
        }
        let refreshed = auth::refresh(&guard)
            .await
            .map_err(|error| LlmError::Config(format!("ChatGPT 令牌刷新失败：{error}")))?;
        auth::save(&refreshed)
            .map_err(|error| LlmError::Config(format!("ChatGPT 令牌写回失败：{error}")))?;
        *guard = refreshed.clone();
        Ok(refreshed)
    }

    fn endpoint(&self) -> String {
        format!("{}/responses", auth::API_BASE)
    }
}

#[async_trait]
impl LlmProvider for ChatGptProvider {
    fn name(&self) -> &str {
        "openai-chatgpt"
    }

    fn default_model(&self) -> &str {
        &self.model
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        // 复用流式实现：SSE 与一次性响应是同一批 item 的两种投递方式，
        // 各写一份解析只会多一份走偏的机会。
        //
        // `tx = None` 表示"只累加、不转发"——`chat_stream_inner` 内部本来就在
        // 喂 `ChatStreamAccumulator`，所以非流式路径的终值就是它的 `finish()`。
        self.chat_stream_inner(request, None).await
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        tx: mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError> {
        self.chat_stream_inner(request, Some(tx)).await
    }
}

/// `chat_stream` 的内部实现：`tx` 为 `None` 时只解析不投递（给 `chat()` 用）。
impl ChatGptProvider {
    /// 发请求，**只在连接层失败时**重试。
    ///
    /// # 为什么重试是安全的
    ///
    /// `is_connect()` 为真表示连接没建立起来（DNS / TCP / TLS / 代理 CONNECT
    /// 任一环节失败）——请求字节一个都没到上游。这类失败是瞬时抖动
    /// （节点切换、代理重启）的典型形态，重试能直接吸收掉。
    ///
    /// 反过来，body 发到一半断掉时 `is_connect()` 为假——那种情况**不能**
    /// 重试：上游可能已经开始生成，重发等于让模型说两遍。
    ///
    /// 实测背景：线上出现过一次 756ms 的 `error sending request`，而连接
    /// 超时是 15s——说明失败发生在建连途中而非等超时，正是这里处理的情形。
    async fn send_with_retry(
        &self,
        auth: &ChatGptAuth,
        payload: Vec<u8>,
    ) -> Result<reqwest::Response, LlmError> {
        const ATTEMPTS: u32 = 3;
        let mut last: Option<String> = None;
        for attempt in 1..=ATTEMPTS {
            let outcome = self
                .client
                .post(self.endpoint())
                .bearer_auth(&auth.access_token)
                .header("chatgpt-account-id", &auth.account_id)
                // 官方 CLI 的身份标识（二进制里的字面量）。如实上报，不伪装成
                // 别的软件——伪装只会更容易被判定异常。
                .header("originator", "codex_cli_rs")
                .header("accept", "text/event-stream")
                .body(payload.clone())
                .send()
                .await;
            match outcome {
                Ok(response) => return Ok(response),
                Err(error) => {
                    let described = auth::describe_transport_error(&error);
                    let retryable = error.is_connect() && attempt < ATTEMPTS;
                    if !retryable {
                        return Err(LlmError::Http(described));
                    }
                    tracing::warn!(attempt, %described, "chatgpt 连接失败，重试");
                    last = Some(described);
                    // 抖动通常几百毫秒就过去；退避固定值即可，太复杂反而难猜。
                    tokio::time::sleep(std::time::Duration::from_millis(400 * attempt as u64))
                        .await;
                }
            }
        }
        // 循环必然在内部 return，这里只是给类型系统一个出口。
        Err(LlmError::Http(last.unwrap_or_else(|| "请求发送失败".into())))
    }

    async fn chat_stream_inner(
        &self,
        request: &ChatRequest,
        tx: Option<mpsc::UnboundedSender<ChatChunk>>,
    ) -> Result<ChatResponse, LlmError> {
        let auth = self.fresh_auth().await?;
        let body = build_request_body(request, &self.model, self.reasoning_effort);
        // 请求体先序列化成字节：重试时每次重建 RequestBuilder（它不可 Clone），
        // 序列化只做一次。
        let payload = serde_json::to_vec(&body)
            .map_err(|error| LlmError::Parse(format!("请求体序列化失败：{error}")))?;
        let response = match self.send_with_retry(&auth, payload).await {
            Ok(response) => response,
            Err(error) => return Err(error),
        };

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            // 地区封锁 / client 被拒这类"不是请求写错了"的失败，交给统一的
            // 翻译给出可执行动作（配代理 / 改用官方客户端）。
            let explained = auth::explain_http_error(status.as_u16(), &text);
            let hint = if status.as_u16() == 401 {
                "\n（ChatGPT 登录可能已失效：重新运行 `echo-agent-core login-chatgpt`）"
            } else {
                ""
            };
            return Err(LlmError::Api(format!("{explained}{hint}")));
        }

        let mut accumulator = ChatStreamAccumulator::new();
        let mut stream = response.bytes_stream();
        // SSE 行可能跨 chunk 切开：必须按行缓冲，不能假设一次读到的就是完整行。
        let mut buffer = String::new();
        while let Some(next) = tokio::time::timeout(READ_IDLE_TIMEOUT, stream.next()).await.map_err(
            |_| {
                LlmError::Http(format!(
                    "read idle timeout: no bytes from upstream for {}s",
                    READ_IDLE_TIMEOUT.as_secs()
                ))
            },
        )? {
            let bytes = next.map_err(|error| LlmError::Http(error.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim_end_matches('\r').to_string();
                buffer.drain(..newline + 1);
                let Some(data) = line.strip_prefix("data:") else {
                    // `event:` 行与空行（事件分隔符）都不带载荷。
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let event: Value = match serde_json::from_str(data) {
                    Ok(event) => event,
                    Err(error) => {
                        // 单条坏事件不该毒化整条流（上游会加新字段/新形状）；
                        // 但也不能静默——记下来，正文缺失时据此排查。
                        tracing::warn!(%error, raw = %echo_defs::token::truncate(data, 200),
                            "skip malformed responses event");
                        continue;
                    }
                };
                apply_event(&event, &mut accumulator, tx.as_ref())?;
            }
        }
        Ok(accumulator.finish())
    }
}

/// 把一个 SSE 事件折进累加器（并投递增量）。
///
/// 未知 `type` 静默跳过：上游加新事件是常态，硬失败等于"上游一升级就断"。
fn apply_event(
    event: &Value,
    accumulator: &mut ChatStreamAccumulator,
    tx: Option<&mpsc::UnboundedSender<ChatChunk>>,
) -> Result<(), LlmError> {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
    let mut emit = |chunk: ChatChunk| {
        accumulator.push(&chunk);
        if let Some(tx) = tx {
            // 接收端关闭（`chat()` 的收集器已完成）不算错误。
            let _ = tx.send(chunk);
        }
    };
    match kind {
        "response.output_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    emit(ChatChunk {
                        content_delta: Some(delta.to_string()),
                        reasoning_delta: None,
                        tool_call_delta: None,
                    });
                }
            }
        }
        // 推理**摘要**（reasoning summary）：Responses API 不把原始思维链
        // 给客户端，只给摘要。映射到 reasoning_delta 是语义上最接近的一档
        // （面板的「思考」块显示的就是这个）。
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    emit(ChatChunk {
                        content_delta: None,
                        reasoning_delta: Some(delta.to_string()),
                        tool_call_delta: None,
                    });
                }
            }
        }
        "response.output_item.added" => {
            let Some(item) = event.get("item") else {
                return Ok(());
            };
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                return Ok(());
            }
            // 工具序号用 `output_index`：同一个响应里可能有多个并行调用，
            // 而 `item.id` 是字符串、累加器要的是下标。
            let index = event
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            emit(ChatChunk {
                content_delta: None,
                reasoning_delta: None,
                tool_call_delta: Some(ToolCallDelta {
                    index,
                    // `call_id` 是最终要回填给 `function_call_output` 的 id；
                    // `id` 是 item id，上游两者都给，取 call_id。
                    id: item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    arguments: None,
                }),
            });
        }
        "response.function_call_arguments.delta" => {
            let index = event
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    emit(ChatChunk {
                        content_delta: None,
                        reasoning_delta: None,
                        tool_call_delta: Some(ToolCallDelta {
                            index,
                            id: None,
                            name: None,
                            arguments: Some(delta.to_string()),
                        }),
                    });
                }
            }
        }
        "response.completed" => {
            let response = event.get("response");
            if let Some(status) = response
                .and_then(|r| r.get("status"))
                .and_then(Value::as_str)
            {
                accumulator.set_stop_reason(stop_reason_for(status));
            }
            if let Some(usage) = response.and_then(|r| r.get("usage")) {
                accumulator.set_usage(usage_of(usage));
            }
        }
        // `incomplete` 是**正常终态**（max_tokens / content_filter），
        // 与 `completed` 同样要取 usage 与状态，否则长度截断会被报成失败。
        "response.incomplete" => {
            let response = event.get("response");
            let reason = response
                .and_then(|r| r.get("incomplete_details"))
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("incomplete");
            accumulator.set_stop_reason(reason.to_string());
            if let Some(usage) = response.and_then(|r| r.get("usage")) {
                accumulator.set_usage(usage_of(usage));
            }
        }
        "response.failed" => {
            let message = event
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("upstream reported response.failed");
            return Err(LlmError::Api(message.to_string()));
        }
        "error" => {
            let message = event
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("upstream stream error");
            return Err(LlmError::Api(message.to_string()));
        }
        _ => {}
    }
    Ok(())
}

/// Responses 的 `status` → 我们的 `stop_reason` 口径（与其它 provider 一致）。
fn stop_reason_for(status: &str) -> String {
    match status {
        "incomplete" => "incomplete".into(),
        "failed" => "failed".into(),
        "cancelled" => "cancelled".into(),
        _ => "end_turn".into(),
    }
}

fn usage_of(usage: &Value) -> echo_defs::message::Usage {
    let get = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0) as u32;
    echo_defs::message::Usage {
        prompt_tokens: get("input_tokens"),
        completion_tokens: get("output_tokens"),
        // 缓存命中：Responses 把它放在 `input_tokens_details.cached_tokens`。
        prompt_cache_hit_tokens: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        prompt_cache_miss_tokens: None,
        prompt_cache_creation_tokens: None,
    }
}

/// `ChatRequest` → Responses 请求体。
///
/// 公开（`pub(crate)`）是为了能被单测直接断言形状——这段映射是整条链路里
/// 最容易悄悄错的地方（系统提示词位置、工具调用形态），而运行期错了只会得到
/// 一个语义诡异的回答。
pub(crate) fn build_request_body(
    request: &ChatRequest,
    model: &str,
    effort: echo_defs::ReasoningEffort,
) -> Value {
    let messages = &request.messages;
    // 系统提示词必须走 `instructions`：Responses 的 input 里没有 system 角色。
    // 多条 system 按空行拼接（本仓只会有 1~2 条：人格提示 + 技能注入）。
    let instructions: Vec<&str> = messages
        .iter()
        .filter(|message| message.role == ChatRole::System)
        .map(|message| message.content.as_str())
        .filter(|content| !content.is_empty())
        .collect();
    let mut input: Vec<Value> = Vec::new();
    for message in messages {
        match message.role {
            ChatRole::System => {}
            ChatRole::User => input.push(json!({
                "type": "message",
                "role": "user",
                "content": content_parts(message),
            })),
            ChatRole::Assistant => {
                // 正文与工具调用是两类 item：正文进 message，调用进顶层
                // `function_call`。工具循环的第二轮如果不这么拆，上游会认为
                // 那是一次"模型输出的文本"而不是"待回填的调用"。
                if !message.content.trim().is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": message.content }],
                    }));
                }
                for call in message.tool_calls.iter().flatten() {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    }));
                }
            }
            ChatRole::Tool => input.push(json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id.clone().unwrap_or_default(),
                "output": message.content,
            })),
        }
    }

    let mut body = json!({
        "model": model,
        "input": input,
        // 订阅通道**不允许**服务端存响应（官方 CLI 也是关的）。开着会让
        // 上游尝试落库并可能直接拒。
        "store": false,
        "stream": true,
        "parallel_tool_calls": true,
        // 这里**不要**放 `include: ["reasoning_summary_text"]`。
        //
        // 推理摘要由下面的 `reasoning.summary: "auto"` 控制，`include` 是另一
        // 回事（它列举响应里要额外带回的**载荷**，合法值形如
        // `reasoning.encrypted_content`）。写错值会被上游整条拒掉：
        // `Invalid value: 'reasoning_summary_text'`，且报错出现在"发消息"这一步，
        // 看起来像网络/额度问题——实际是请求体字段非法。
    });
    if !instructions.is_empty() {
        body["instructions"] = json!(instructions.join("\n\n"));
    }
    if let Some(tools) = &request.tools {
        if !tools.is_empty() {
            body["tools"] = json!(tools
                .iter()
                .map(|tool| json!({
                    // Responses 的函数工具是**扁平**的（name 在顶层），
                    // 不是 chat/completions 的 `{"function": {...}}` 嵌套。
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    // `parameters` 是 Option：None 时给一个空对象 schema，
                    // 少一个必填字段上游会整条请求 400。
                    "parameters": tool
                        .parameters
                        .clone()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                }))
                .collect::<Vec<_>>());
            body["tool_choice"] = json!("auto");
        }
    }
    // 本仓的三档（Low/High/Max）映射到上游的 low/high/xhigh：
    // `max` 是本仓"尽量深"的默认档，对应上游的最高档 xhigh。
    let level = match effort {
        echo_defs::ReasoningEffort::Low => "low",
        echo_defs::ReasoningEffort::High => "high",
        echo_defs::ReasoningEffort::Max => "xhigh",
    };
    body["reasoning"] = json!({ "effort": level, "summary": "auto" });
    body
}

/// 用户消息的 content：有图时是 part 数组，纯文本时退化成字符串。
///
/// 退化不是微优化：纯文本走字符串时上游的 token 计数与缓存命中更稳定，
/// 而 part 数组会被当作"结构化输入"另算。
fn content_parts(message: &ChatMessage) -> Value {
    if message.images.is_empty() {
        return json!(message.content);
    }
    let mut parts: Vec<Value> = Vec::new();
    if !message.content.is_empty() {
        parts.push(json!({ "type": "input_text", "text": message.content }));
    }
    for image in &message.images {
        parts.push(json!({ "type": "input_image", "image_url": image }));
    }
    json!(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_defs::message::ToolCall;

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "gpt-5-codex".into(),
            messages,
            tools: None,
            temperature: None,
            max_tokens: None,
        }
    }

    /// 系统提示词必须进 `instructions`，且**不能**留在 input 里。
    /// 留在 input 里上游会认为它是一条 user/assistant 消息，能力大幅弱化
    /// （而且是静默的：回答还像样，只是不再遵守）。
    #[test]
    fn system_prompt_becomes_instructions() {
        let body = build_request_body(
            &request(vec![
                ChatMessage::system("你是 Alix"),
                ChatMessage::user("你好"),
            ]),
            "gpt-5-codex",
            echo_defs::ReasoningEffort::High,
        );
        assert_eq!(body["instructions"], "你是 Alix");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1, "system 不该出现在 input: {input:?}");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "你好");
    }

    /// 工具调用必须是**顶层 function_call item**，不能塞在 assistant 的
    /// 消息里——塞进去上游当普通文本，工具循环第二轮就断。
    #[test]
    fn assistant_tool_calls_become_top_level_items() {
        let assistant = ChatMessage {
            role: ChatRole::Assistant,
            content: String::new(),
            reasoning_content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            }]),
            tool_call_id: None,
            images: vec![],
        };
        let tool_result = ChatMessage {
            role: ChatRole::Tool,
            content: "fn main() {}".into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            images: vec![],
        };
        let body = build_request_body(
            &request(vec![
                ChatMessage::user("读 a.rs"),
                assistant,
                tool_result,
            ]),
            "gpt-5-codex",
            echo_defs::ReasoningEffort::High,
        );
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["name"], "read_file");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "fn main() {}");
    }

    /// 工具声明是扁平的（name 在顶层），不是 chat/completions 的嵌套形状。
    #[test]
    fn tools_are_flat() {
        let mut req = request(vec![ChatMessage::user("hi")]);
        req.tools = Some(vec![echo_defs::tool::ToolDefinition {
            name: "list_files".into(),
            description: "列出文件".into(),
            parameters: Some(json!({"type": "object", "properties": {}})),
        }]);
        let body = build_request_body(&req, "m", echo_defs::ReasoningEffort::High);
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "list_files");
        assert!(tool.get("function").is_none(), "不该是嵌套形状");
        assert_eq!(body["tool_choice"], "auto");
    }

    /// 订阅通道必须 `store: false`。
    #[test]
    fn store_is_disabled() {
        let body = build_request_body(
            &request(vec![ChatMessage::user("hi")]),
            "m",
            echo_defs::ReasoningEffort::High,
        );
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn reasoning_effort_maps_to_level() {
        let body = build_request_body(
            &request(vec![ChatMessage::user("hi")]),
            "m",
            echo_defs::ReasoningEffort::High,
        );
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    /// 图片走 part 数组；纯文本退化成字符串（上游计数与缓存更稳）。
    #[test]
    fn images_become_input_image_parts() {
        let mut message = ChatMessage::user("看图");
        message.images = vec!["data:image/png;base64,AAA".into()];
        let body = build_request_body(
            &request(vec![message]),
            "m",
            echo_defs::ReasoningEffort::High,
        );
        let content = &body["input"][0]["content"];
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
    }

    fn chunk_of(event: Value) -> (ChatChunk, Option<String>, Option<echo_defs::message::Usage>) {
        let mut accumulator = ChatStreamAccumulator::new();
        apply_event(&event, &mut accumulator, None).unwrap();
        // 用单条事件时的差量还原（累加器只给终值，所以这里直接断言 delta 通道）。
        let delta = ChatChunk {
            content_delta: event
                .get("delta")
                .and_then(Value::as_str)
                .filter(|_| event["type"] == "response.output_text.delta")
                .map(str::to_string),
            reasoning_delta: event
                .get("delta")
                .and_then(Value::as_str)
                .filter(|_| event["type"] == "response.reasoning_summary_text.delta")
                .map(str::to_string),
            tool_call_delta: None,
        };
        let _ = accumulator;
        (delta, None, None)
    }

    #[test]
    fn text_delta_reaches_accumulator() {
        let mut accumulator = ChatStreamAccumulator::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        apply_event(
            &json!({"type": "response.output_text.delta", "delta": "你好"}),
            &mut accumulator,
            Some(&tx),
        )
        .unwrap();
        let chunk = rx.try_recv().expect("delta should be forwarded");
        assert_eq!(chunk.content_delta.as_deref(), Some("你好"));
    }

    #[test]
    fn reasoning_summary_delta_maps_to_reasoning() {
        let mut accumulator = ChatStreamAccumulator::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        apply_event(
            &json!({"type": "response.reasoning_summary_text.delta", "delta": "想一下"}),
            &mut accumulator,
            Some(&tx),
        )
        .unwrap();
        let chunk = rx.try_recv().unwrap();
        assert_eq!(chunk.reasoning_delta.as_deref(), Some("想一下"));
        assert!(chunk.content_delta.is_none());
    }

    /// 工具调用的 id 与参数分两个事件到达——这是 Responses 的常态，
    /// 任何一半丢了都会让工具循环回填错误的 call_id。
    #[test]
    fn tool_call_id_and_arguments_arrive_separately() {
        let mut accumulator = ChatStreamAccumulator::new();
        apply_event(
            &json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_9", "name": "read_file"}
            }),
            &mut accumulator,
            None,
        )
        .unwrap();
        apply_event(
            &json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{\"path\":"
            }),
            &mut accumulator,
            None,
        )
        .unwrap();
        apply_event(
            &json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "\"a.rs\"}"
            }),
            &mut accumulator,
            None,
        )
        .unwrap();
        let response = accumulator.finish();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_9");
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
    }

    /// 非 function_call 的 item（message / reasoning）不该被当成工具。
    #[test]
    fn non_function_items_are_ignored() {
        let mut accumulator = ChatStreamAccumulator::new();
        apply_event(
            &json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "reasoning", "id": "rs_1"}
            }),
            &mut accumulator,
            None,
        )
        .unwrap();
        assert!(accumulator.finish().tool_calls.is_empty());
    }

    #[test]
    fn completed_event_records_usage_and_stop_reason() {
        let mut accumulator = ChatStreamAccumulator::new();
        apply_event(
            &json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "usage": {"input_tokens": 120, "output_tokens": 8,
                              "input_tokens_details": {"cached_tokens": 64}}
                }
            }),
            &mut accumulator,
            None,
        )
        .unwrap();
        let response = accumulator.finish();
        assert_eq!(response.usage.prompt_tokens, 120);
        assert_eq!(response.usage.completion_tokens, 8);
        assert_eq!(response.usage.prompt_cache_hit_tokens, Some(64));
        assert_eq!(response.stop_reason.as_deref(), Some("end_turn"));
    }

    /// `incomplete` 是正常终态（长度截断），不能当失败。
    #[test]
    fn incomplete_is_not_an_error() {
        let mut accumulator = ChatStreamAccumulator::new();
        apply_event(
            &json!({
                "type": "response.incomplete",
                "response": {"incomplete_details": {"reason": "max_output_tokens"},
                             "usage": {"input_tokens": 1, "output_tokens": 2}}
            }),
            &mut accumulator,
            None,
        )
        .unwrap();
        let response = accumulator.finish();
        assert_eq!(response.stop_reason.as_deref(), Some("max_output_tokens"));
    }

    #[test]
    fn failed_event_is_an_error_with_upstream_message() {
        let mut accumulator = ChatStreamAccumulator::new();
        let error = apply_event(
            &json!({
                "type": "response.failed",
                "response": {"error": {"message": "quota exhausted"}}
            }),
            &mut accumulator,
            None,
        )
        .unwrap_err();
        assert!(format!("{error}").contains("quota exhausted"));
    }

    /// 新事件类型不该让整条流失败（上游加事件是常态）。
    #[test]
    fn unknown_event_is_tolerated() {
        let mut accumulator = ChatStreamAccumulator::new();
        apply_event(
            &json!({"type": "response.some_brand_new_event", "delta": "x"}),
            &mut accumulator,
            None,
        )
        .unwrap();
        assert!(accumulator.finish().content.is_none());
    }

    /// 保留未使用但有意留下的辅助（避免 dead_code 噪声）。
    #[test]
    fn chunk_helper_is_wired() {
        let (delta, _, _) =
            chunk_of(json!({"type": "response.output_text.delta", "delta": "x"}));
        assert_eq!(delta.content_delta.as_deref(), Some("x"));
    }

    // ── 传输错误可诊断性（2026-11 线上事故）─────────────────────────
    //
    // 用户报「model request failed: HTTP 请求失败: error sending request for
    // url (…codex/responses)」——只有这一句，看不出卡在哪一层。真实原因藏在
    // reqwest 的 source 链里，被 `{error}` 压掉了。

    /// 传输错误必须带上 source 链：DNS / TCP / TLS / 代理 CONNECT 四种原因
    /// 的 Display 措辞完全相同，只有链里的下层错误能区分。
    #[tokio::test]
    async fn transport_error_includes_source_chain() {
        // 连一个必然无人监听的端口：拿到真实的 reqwest::Error（构造不出来，
        // 只能真发一次）。回环连接被拒是毫秒级的，不依赖外网。
        let error = reqwest::Client::new()
            .post("http://127.0.0.1:1/responses")
            .send()
            .await
            .expect_err("连 1 端口不可能成功");
        assert!(error.is_connect(), "回环拒绝应被判定为连接层失败");

        let described = auth::describe_transport_error(&error);
        // 至少两层：外层 "error sending request"，内层 connect 细节。
        let arrows = described.matches(" → ").count();
        assert!(arrows >= 1, "应展开 source 链，实际只有一层：{described}");
        assert!(
            described.contains("连接层失败=true"),
            "必须标出可重试性：{described}"
        );
        // 平台差异（Linux 是 Connection refused，别的平台措辞不同），
        // 所以只断言"有下层细节"，不断言具体单词。
        assert!(
            described.lines().next().unwrap().contains("http://127.0.0.1:1"),
            "首行应保留原始 URL：{described}"
        );
    }

    /// 代理配错时**建 provider 就失败**，不能静默退化成直连。
    ///
    /// 退化后的表现是"用户发消息时报网络错误"，而真正原因是一个拼错的
    /// 代理 URL——排查方向会被完全带偏。
    #[tokio::test]
    async fn provider_construction_surfaces_invalid_proxy() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("ECHO_CHATGPT_PROXY", "::not a url::") };
        // 令牌内容与本次断言无关：宁可让它在 build_client 之前就失败。
        let auth = ChatGptAuth {
            access_token: "t".into(),
            refresh_token: String::new(),
            id_token: String::new(),
            account_id: "a".into(),
            expires_at_ms: i64::MAX,
            email: String::new(),
        };
        let error = match ChatGptProvider::new(auth, "gpt-5-codex") {
            Ok(_) => panic!("非法代理必须让构造失败，不能静默直连"),
            Err(error) => error,
        };
        unsafe { std::env::remove_var("ECHO_CHATGPT_PROXY") };
        let message = error.to_string();
        assert!(message.contains("代理地址无效"), "{message}");
    }


    /// 请求体**不能**带 `include: ["reasoning_summary_text"]`。
    ///
    /// 这个字段值上游不认（合法值形如 `reasoning.encrypted_content`），会整条
    /// 400：`Invalid value: 'reasoning_summary_text'`。而它看起来像是"要摘要"，
    /// 很容易被当成正确写法加回来——所以钉一条测试。
    ///
    /// 摘要的真正开关是 `reasoning.summary`，那条断言在下面一起守着：
    /// 删 include 时若把摘要也删了，面板的「思考」块就空了。
    #[test]
    fn request_body_does_not_send_unsupported_include() {
        let body = build_request_body(
            &request(vec![ChatMessage::user("hi")]),
            "gpt-6.1-sol",
            echo_defs::ReasoningEffort::High,
        );
        assert!(
            body.get("include").is_none(),
            "include 会被上游拒（该值不在支持列表里）：{body}"
        );
        assert_eq!(
            body["reasoning"]["summary"], "auto",
            "推理摘要靠 reasoning.summary 控制，不能被顺手删掉：{body}"
        );
    }

}
