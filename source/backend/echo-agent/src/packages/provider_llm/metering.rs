//! 计量 provider 装饰器：LLM 出口的 token 用量记录（余额/用量图表数据源）。
//!
//! [`MeteringProvider`] 实现 [`LlmProvider`]，在每次 `chat` **成功后**把
//! `usage`（prompt / completion tokens）追加进进程级 [`MetricsStore`]。
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

use crate::metrics::MetricsStore;

/// 用计量装饰器包装 provider（返回新句柄，原句柄不受影响）。
pub fn wrap_metering(
    inner: Arc<dyn LlmProvider>,
    store: Arc<MetricsStore>,
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
    store: Arc<MetricsStore>,
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
        );
        Ok(response)
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        tx: mpsc::UnboundedSender<ChatChunk>,
    ) -> Result<(), LlmError> {
        // 流式路径不做计量：chunk 协议不携带收尾总账，核心调用路径统一走
        // `chat`（与脱敏装饰器的流式透传口径一致）。
        self.inner.chat_stream(request, tx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_defs::message::Usage;
    use std::sync::Mutex;

    /// 固定返回指定 usage 的假 provider。
    struct FixedUsageProvider {
        usage: Usage,
        seen_models: Mutex<Vec<String>>,
    }

    impl FixedUsageProvider {
        fn new(prompt: u32, completion: u32) -> Self {
            Self {
                usage: Usage {
                    prompt_tokens: prompt,
                    completion_tokens: completion,
                },
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
        ) -> Result<(), LlmError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn chat_records_usage_with_request_model() {
        let dir = std::env::temp_dir().join(format!(
            "echo-metering-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let store = Arc::new(MetricsStore::new(&dir));
        let inner = Arc::new(FixedUsageProvider::new(1200, 340));
        let provider = wrap_metering(inner, store.clone(), "deepseek".into());

        let request = ChatRequest {
            model: "deepseek-flash".into(),
            messages: vec![],
            tools: None,
            temperature: None,
            max_tokens: None,
        };
        let response = provider.chat(&request).await.expect("chat ok");
        assert_eq!(response.usage.prompt_tokens, 1200);

        let slices = store.usage_slices("deepseek", 0);
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].model, "deepseek-flash");
        assert_eq!(slices[0].prompt_tokens, 1200);
        assert_eq!(slices[0].completion_tokens, 340);
        assert_eq!(slices[0].calls, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
