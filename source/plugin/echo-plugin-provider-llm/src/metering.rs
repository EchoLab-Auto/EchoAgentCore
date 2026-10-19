//! 计量 provider 装饰器：LLM 出口的 token 用量记录（余额/用量图表数据源）。
//!
//! [`MeteringProvider`] 实现 [`LlmProvider`]，在每次 `chat` **成功后**把
//! `usage`（prompt / completion tokens）交给宿主注入的 [`UsageRecorder`]。
//! 覆盖所有经包装 provider 的调用：内置循环、echo-loop、等待回复、压缩、
//! 子代理——组合根把它们接到同一被包装句柄（与脱敏装饰器同一装配点）。
//!
//! 归属口径：`profile` 在**包装构建时**快照——provider 在每次 API 切换 /
//! persona 解析时重建，快照即当时生效的配置名（`""` = 顶层默认；与
//! `QueryApiBalance` 的 `name` 同命名空间）。`model` 取每次请求的
//! `request.model`（模型切换不重建 provider，故不能快照）。

use std::sync::Arc;

use async_trait::async_trait;
use echo_defs::llm::{LlmError, LlmProvider};
use echo_defs::message::{ChatChunk, ChatRequest, ChatResponse};
use tokio::sync::mpsc;

/// 用量记录出口（宿主把指标存储实现成该 trait 注入）。
///
/// 只有这一个方法：本 crate 不认识存储形状（文件 / 内存 / 远端），
/// 也不认识成本估算与保留期策略——那些属宿主指标子系统。
pub trait UsageRecorder: Send + Sync {
    /// 记录一次 LLM 调用的 token 用量。
    ///
    /// - `profile`：生效的 API profile 名（`""` = 顶层默认）；
    /// - `model`：本次请求的模型名；
    /// - `prompt` / `completion`：输入（含缓存）/ 输出 token；
    /// - `cache_hit`：命中缓存的输入 token 子集。
    fn record_usage(
        &self,
        profile: &str,
        model: &str,
        prompt: u32,
        completion: u32,
        cache_hit: u32,
    );
}

/// 用计量装饰器包装 provider（返回新句柄，原句柄不受影响）。
pub fn wrap_metering(
    inner: Arc<dyn LlmProvider>,
    store: Arc<dyn UsageRecorder>,
    profile: String,
) -> Arc<dyn LlmProvider> {
    Arc::new(MeteringProvider {
        inner,
        store,
        profile,
    })
}

struct MeteringProvider {
    inner: Arc<dyn LlmProvider>,
    store: Arc<dyn UsageRecorder>,
    profile: String,
}

#[async_trait]
impl LlmProvider for MeteringProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn default_model(&self) -> &str {
        self.inner.default_model()
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let response = self.inner.chat(request).await?;
        self.store.record_usage(
            &self.profile,
            &request.model,
            response.usage.prompt_tokens,
            response.usage.completion_tokens,
            response.usage.prompt_cache_hit_tokens.unwrap_or(0),
        );
        Ok(response)
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        tx: mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<ChatResponse, LlmError> {
        // 流式路径同样计量：provider 在流内累积 usage，随组装响应返回
        // （`stream_options.include_usage` / message_start+delta）。
        let response = self.inner.chat_stream(request, tx).await?;
        self.store.record_usage(
            &self.profile,
            &request.model,
            response.usage.prompt_tokens,
            response.usage.completion_tokens,
            response.usage.prompt_cache_hit_tokens.unwrap_or(0),
        );
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_defs::message::Usage;
    use std::sync::Mutex;

    /// 一次上报的快照：(profile, model, prompt, completion, cache_hit)。
    type RecordedCall = (String, String, u32, u32, u32);

    /// 记录所有上报的假出口（替代宿主的 MetricsStore）。
    #[derive(Default)]
    struct RecordingRecorder {
        calls: Mutex<Vec<RecordedCall>>,
    }

    impl RecordingRecorder {
        fn last(&self) -> RecordedCall {
            self.calls
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("recorded")
        }
        fn count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl UsageRecorder for RecordingRecorder {
        fn record_usage(
            &self,
            profile: &str,
            model: &str,
            prompt: u32,
            completion: u32,
            cache_hit: u32,
        ) {
            self.calls.lock().unwrap().push((
                profile.to_string(),
                model.to_string(),
                prompt,
                completion,
                cache_hit,
            ));
        }
    }

    /// 固定返回指定 usage 的假 provider。
    struct FixedUsageProvider {
        usage: Usage,
        seen_models: Mutex<Vec<String>>,
    }

    impl FixedUsageProvider {
        fn new(usage: Usage) -> Self {
            Self {
                usage,
                seen_models: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for FixedUsageProvider {
        fn name(&self) -> &str {
            "fixed"
        }
        fn default_model(&self) -> &str {
            "fixed-model"
        }
        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            self.seen_models.lock().unwrap().push(request.model.clone());
            Ok(ChatResponse {
                content: Some("ok".into()),
                reasoning_content: None,
                tool_calls: vec![],
                usage: self.usage,
                stop_reason: None,
            })
        }
        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            _tx: mpsc::UnboundedSender<ChatChunk>,
        ) -> Result<ChatResponse, LlmError> {
            Ok(ChatResponse::empty())
        }
    }

    fn request(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![],
            tools: None,
            temperature: None,
            max_tokens: None,
        }
    }

    #[tokio::test]
    async fn chat_records_usage_with_request_model() {
        let recorder = Arc::new(RecordingRecorder::default());
        let provider = wrap_metering(
            Arc::new(FixedUsageProvider::new(Usage {
                prompt_tokens: 1200,
                completion_tokens: 300,
                prompt_cache_hit_tokens: None,
                prompt_cache_miss_tokens: None,
                prompt_cache_creation_tokens: None,
            })),
            recorder.clone(),
            String::new(),
        );
        provider.chat(&request("m-1")).await.expect("chat ok");
        assert_eq!(recorder.count(), 1);
        let (profile, model, prompt, completion, cache_hit) = recorder.last();
        // 空 profile = 顶层默认（宿主口径）
        assert_eq!(profile, "");
        // model 取请求里的，不是 provider 的 default_model（模型可热切换）
        assert_eq!(model, "m-1");
        assert_eq!((prompt, completion, cache_hit), (1200, 300, 0));
    }

    #[tokio::test]
    async fn cache_hit_tokens_are_forwarded_not_folded() {
        let recorder = Arc::new(RecordingRecorder::default());
        let provider = wrap_metering(
            Arc::new(FixedUsageProvider::new(Usage {
                prompt_tokens: 1000,
                completion_tokens: 100,
                prompt_cache_hit_tokens: Some(400),
                prompt_cache_miss_tokens: Some(600),
                prompt_cache_creation_tokens: None,
            })),
            recorder.clone(),
            "deepseek".into(),
        );
        provider
            .chat(&request("deepseek-flash"))
            .await
            .expect("chat ok");
        let (profile, model, prompt, completion, cache_hit) = recorder.last();
        assert_eq!(profile, "deepseek");
        assert_eq!(model, "deepseek-flash");
        // prompt 为**总输入**（含缓存），cache_hit 是其子集——两者都不折叠
        assert_eq!((prompt, completion, cache_hit), (1000, 100, 400));
    }

    #[tokio::test]
    async fn provider_name_and_model_pass_through() {
        let recorder = Arc::new(RecordingRecorder::default());
        let provider = wrap_metering(
            Arc::new(FixedUsageProvider::new(Usage::default())),
            recorder,
            "p".into(),
        );
        assert_eq!(provider.name(), "fixed");
        assert_eq!(provider.default_model(), "fixed-model");
    }

    #[tokio::test]
    async fn failed_chat_records_nothing() {
        struct Failing;
        #[async_trait]
        impl LlmProvider for Failing {
            fn name(&self) -> &str {
                "failing"
            }
            fn default_model(&self) -> &str {
                "failing"
            }
            async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, LlmError> {
                Err(LlmError::Http("boom".into()))
            }
            async fn chat_stream(
                &self,
                _request: &ChatRequest,
                _tx: mpsc::UnboundedSender<ChatChunk>,
            ) -> Result<ChatResponse, LlmError> {
                Err(LlmError::Http("boom".into()))
            }
        }
        let recorder = Arc::new(RecordingRecorder::default());
        let provider = wrap_metering(Arc::new(Failing), recorder.clone(), "p".into());
        assert!(provider.chat(&request("m")).await.is_err());
        assert_eq!(recorder.count(), 0);
    }
}
