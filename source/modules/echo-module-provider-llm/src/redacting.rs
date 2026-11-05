//! 脱敏 provider 装饰器：LLM 请求出口的敏感信息卡口。
//!
//! [`RedactingProvider`] 实现 [`LlmProvider`]，在请求到达真实 provider
//! **之前**对消息全文脱敏——覆盖内置循环、echo-loop、子代理与等待回复的
//! 全部 LLM 出口（它们拿到的都是同一被包装的 provider）。这是"秘密永不
//! 到达第三方 API"的最终保证层：即使上游（工具 / 入站 / 热更新）有漏，
//! 密钥也不会离开本机。
//!
//! 说明（2026-10）：
//! - 只处理**请求侧**。响应侧（模型输出）无需脱敏——上下文已被保证干净，
//!   模型不可能复述未见过的值；流式 chunk 逐段扫描还会破坏跨 chunk 的值，
//!   故明确不做；
//! - 请求克隆后改写：脱敏只发生在本机内存，原请求对象不受影响。

use std::sync::Arc;

use async_trait::async_trait;
use echo_defs::llm::{LlmError, LlmProvider};
use echo_defs::message::{ChatChunk, ChatRequest, ChatResponse};
use echo_defs::sanitize::Redactor;
use tokio::sync::mpsc;

/// 用脱敏器包装 provider（返回新句柄，原句柄不受影响）。
///
/// 组合根在**每一处** provider 构建点调用（启动、persona API 切换、
/// API 配置热更新）；禁止裸装配。
pub fn wrap_redacting(
    inner: Arc<dyn LlmProvider>,
    redactor: Arc<dyn Redactor>,
) -> Arc<dyn LlmProvider> {
    Arc::new(RedactingProvider { inner, redactor })
}

/// LLM 请求出口脱敏装饰器（见模块文档）。
struct RedactingProvider {
    inner: Arc<dyn LlmProvider>,
    redactor: Arc<dyn Redactor>,
}

#[async_trait]
impl LlmProvider for RedactingProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn default_model(&self) -> &str {
        self.inner.default_model()
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let clean = redact_request(self.redactor.as_ref(), request);
        self.inner.chat(&clean).await
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        tx: mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError> {
        let clean = redact_request(self.redactor.as_ref(), request);
        self.inner.chat_stream(&clean, tx).await
    }
}

/// 请求级脱敏：逐条消息改写正文 / reasoning / 工具调用参数。
///
/// 工具调用参数也在内：历史里的 assistant 消息会原样回传给模型——
/// 参数若含敏感值（模型不应见到，但防御纵深），同样不得出网。
fn redact_request(redactor: &dyn Redactor, request: &ChatRequest) -> ChatRequest {
    let mut clean = request.clone();
    for message in &mut clean.messages {
        let report = redactor.redact(&message.content);
        if report.changed {
            message.content = report.text;
        }
        if let Some(reasoning) = &message.reasoning_content {
            let report = redactor.redact(reasoning);
            if report.changed {
                message.reasoning_content = Some(report.text);
            }
        }
        if let Some(tool_calls) = &mut message.tool_calls {
            for call in tool_calls {
                let report = redactor.redact(&call.arguments);
                if report.changed {
                    call.arguments = report.text;
                }
            }
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_defs::message::{ChatMessage, ChatRole, ToolCall, Usage};
    use echo_defs::sanitize::{HitKind, PatternError, RedactReport, Redactor, SecretHit};
    use std::sync::Mutex;

    const KEY: &str = "sk-testtesttesttesttesttest01";

    /// 记录请求的 provider（断言"出网前"的最终形态）。
    struct RecordingProvider {
        seen: Mutex<Vec<ChatRequest>>,
    }

    impl RecordingProvider {
        fn new() -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
            }
        }
        fn last(&self) -> ChatRequest {
            self.seen
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("a request")
        }
    }

    #[async_trait]
    impl LlmProvider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }
        fn default_model(&self) -> &str {
            "recording-model"
        }
        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            self.seen.lock().unwrap().push(request.clone());
            Ok(ChatResponse {
                content: Some("ok".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: Usage::default(),
                stop_reason: None,
            })
        }
        async fn chat_stream(
            &self,
            request: &ChatRequest,
            _tx: mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<ChatResponse, LlmError> {
            self.seen.lock().unwrap().push(request.clone());
            Ok(ChatResponse::empty())
        }
    }

    /// 最小假脱敏器：把 KEY 替换为 `[X]`。
    struct FakeRedactor;

    impl Redactor for FakeRedactor {
        fn name(&self) -> &str {
            "fake"
        }
        fn redact(&self, text: &str) -> RedactReport {
            if !text.contains(KEY) {
                return RedactReport {
                    text: text.to_string(),
                    changed: false,
                    hits: vec![],
                };
            }
            let text = text.replace(KEY, "[X]");
            RedactReport {
                text,
                changed: true,
                hits: vec![SecretHit {
                    label: "fake".into(),
                    kind: HitKind::Registered,
                    span: (0, 0),
                }],
            }
        }
        fn scan(&self, text: &str) -> Vec<SecretHit> {
            if text.contains(KEY) {
                vec![SecretHit {
                    label: "fake".into(),
                    kind: HitKind::Registered,
                    span: (0, 0),
                }]
            } else {
                vec![]
            }
        }
        fn register_secret(&self, _value: &str, _label: &str) {}
        fn register_pattern(&self, _name: &str, _pattern: &str) -> Result<(), PatternError> {
            Ok(())
        }
        fn registered_count(&self) -> usize {
            1
        }
    }

    fn request_with(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages,
            tools: None,
            temperature: None,
            max_tokens: None,
        }
    }

    #[tokio::test]
    async fn chat_request_never_leaks_secret() {
        let recorder = Arc::new(RecordingProvider::new());
        let provider = wrap_redacting(recorder.clone(), Arc::new(FakeRedactor));
        let request = request_with(vec![
            ChatMessage::system("sys"),
            ChatMessage {
                role: ChatRole::User,
                content: format!("my key is {KEY} right?"),
                reasoning_content: Some(format!("recall {KEY}")),
                tool_calls: Some(vec![ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: format!(r#"{{"command":"echo {KEY}"}}"#),
                }]),
                tool_call_id: None,
                images: vec![],
            },
        ]);
        let _ = provider.chat(&request).await.expect("chat ok");
        let seen = recorder.last();
        let serialized = format!("{seen:?}");
        assert!(!serialized.contains(KEY), "secret must not reach provider");
        assert!(seen.messages[1].content.contains("[X]"));
    }

    #[tokio::test]
    async fn stream_request_is_redacted_too() {
        let recorder = Arc::new(RecordingProvider::new());
        let provider = wrap_redacting(recorder.clone(), Arc::new(FakeRedactor));
        let request = request_with(vec![ChatMessage::user(format!("key={KEY}"))]);
        let (tx, _rx) = mpsc::unbounded_channel();
        provider.chat_stream(&request, tx).await.expect("stream ok");
        assert!(!format!("{:?}", recorder.last()).contains(KEY));
    }

    #[tokio::test]
    async fn clean_requests_pass_through_unchanged() {
        let recorder = Arc::new(RecordingProvider::new());
        let provider = wrap_redacting(recorder.clone(), Arc::new(FakeRedactor));
        let request = request_with(vec![ChatMessage::user("no secrets here")]);
        let _ = provider.chat(&request).await.expect("chat ok");
        assert_eq!(recorder.last().messages[0].content, "no secrets here");
    }
}
