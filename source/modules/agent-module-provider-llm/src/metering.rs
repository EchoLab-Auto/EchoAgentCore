//! 计量 provider 装饰器：LLM 出口的 token 用量记录（余额/用量图表数据源）。
//!
//! [`MeteringProvider`] 实现 [`LlmProvider`]，把每次 `chat` / `chat_stream` 的
//! 用量交给宿主注入的 [`UsageRecorder`]。覆盖所有经包装 provider 的调用：
//! 内置循环、agent-module-loop、等待回复、压缩、子代理——组合根把它们接到同一
//! 被包装句柄（与脱敏装饰器同一装配点）。
//!
//! ## 失败也记（2026-11 用量盲区修复）
//!
//! 此前只在**成功返回后**记录：失败的上游调用（超时 / 取消 / 5xx / 计费类
//! 4xx）真实消耗了配额，却完全不进图表——用量曲线显著低于实际配额消耗，
//! 用户会以为"没用多少"。现在：
//!
//! - 成功：用上游返回的真实 `usage`（`ok = true`）；
//! - 失败 / 超时 / 被取消：用**本地估算**的输入 token、输出 0（`ok = false`）。
//!   估算值不是账单真值，但把"一次都没记"变成"记了一条估算"，曲线形状才对
//!   得上；输出按 0 记，因为失败时无从知道上游生成了多少。
//!
//! **取消**（future 被 drop）走的是 [`FailureGuard::drop`]：`?` 提前返回和
//! 外部 abort 都会 drop 它，因此这条路径也被覆盖——这是此前最大的盲区。
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
/// 只有一个方法：本 crate 不认识存储形状（文件 / 内存 / 远端），
/// 也不认识成本估算与保留期策略——那些属宿主指标子系统。
pub trait UsageRecorder: Send + Sync {
    /// 记录一次 LLM 调用的 token 用量。
    ///
    /// - `profile`：生效的 API profile 名（`""` = 顶层默认）；
    /// - `model`：本次请求的模型名；
    /// - `prompt` / `completion`：输入（含缓存）/ 输出 token；
    /// - `cache_hit`：命中缓存的输入 token 子集；
    /// - `ok`：调用是否正常返回。`false` = 失败 / 超时 / 取消，此时
    ///   `prompt` 是本地估算值、`completion` 恒为 0（见模块文档）。
    fn record_usage(
        &self,
        profile: &str,
        model: &str,
        prompt: u32,
        completion: u32,
        cache_hit: u32,
        ok: bool,
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

/// 失败兜底：本结构在作用域结束（含 `?` 提前返回、future 被 drop 取消）
/// 时若仍是 `armed`，就补记一条 `ok = false` 的估算用量。
///
/// 为什么不用 `match result` 就够：**取消不会返回**——`chat_stream` 的 future
/// 被落选于 `select!` 或直接被 drop 时，函数体后半段一行都不会执行。
/// 只有 `Drop` 能在那条路径上跑。
struct FailureGuard<'a> {
    store: &'a dyn UsageRecorder,
    profile: &'a str,
    model: &'a str,
    prompt: u32,
    armed: bool,
}

impl Drop for FailureGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // 输出按 0：失败时无从知道上游生成了多少。输入用估算值——
            // "请求已经发出去了"比"什么也没发生"更接近真实配额消耗。
            self.store
                .record_usage(self.profile, self.model, self.prompt, 0, 0, false);
        }
    }
}

/// 请求侧 token 估算（失败时没有上游 `usage` 可用）。
///
/// 与 `echo_defs::token` 同一估算口径（CJK 1 字 ≈ 1 token、base64 串按
/// 1 字符 1 token、其余 3 字符 1 token），工具 schema 也计入——它每步都随
/// 请求重发，漏算会让失败记录系统性偏低。
fn estimate_request_tokens(request: &ChatRequest) -> u32 {
    let mut total = echo_defs::token::estimate_history_tokens(&request.messages);
    if let Some(tools) = &request.tools {
        for tool in tools {
            total += echo_defs::token::estimate_tokens(&tool.name);
            total += echo_defs::token::estimate_tokens(&tool.description);
            if let Some(parameters) = &tool.parameters {
                total += echo_defs::token::estimate_tokens(&parameters.to_string());
            }
        }
    }
    u32::try_from(total).unwrap_or(u32::MAX)
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
        let mut guard = FailureGuard {
            store: self.store.as_ref(),
            profile: &self.profile,
            model: &request.model,
            prompt: estimate_request_tokens(request),
            armed: true,
        };
        let response = self.inner.chat(request).await?;
        guard.armed = false;
        self.store.record_usage(
            &self.profile,
            &request.model,
            response.usage.prompt_tokens,
            response.usage.completion_tokens,
            response.usage.prompt_cache_hit_tokens.unwrap_or(0),
            true,
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
        let mut guard = FailureGuard {
            store: self.store.as_ref(),
            profile: &self.profile,
            model: &request.model,
            prompt: estimate_request_tokens(request),
            armed: true,
        };
        let response = self.inner.chat_stream(request, tx).await?;
        guard.armed = false;
        self.store.record_usage(
            &self.profile,
            &request.model,
            response.usage.prompt_tokens,
            response.usage.completion_tokens,
            response.usage.prompt_cache_hit_tokens.unwrap_or(0),
            true,
        );
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_defs::message::Usage;
    use std::sync::Mutex;

    /// 一次上报的快照：(profile, model, prompt, completion, cache_hit, ok)。
    type RecordedCall = (String, String, u32, u32, u32, bool);

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
            ok: bool,
        ) {
            self.calls.lock().unwrap().push((
                profile.to_string(),
                model.to_string(),
                prompt,
                completion,
                cache_hit,
                ok,
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
        let (profile, model, prompt, completion, cache_hit, ok) = recorder.last();
        // 空 profile = 顶层默认（宿主口径）
        assert_eq!(profile, "");
        // model 取请求里的，不是 provider 的 default_model（模型可热切换）
        assert_eq!(model, "m-1");
        assert_eq!((prompt, completion, cache_hit), (1200, 300, 0));
        assert!(ok, "成功调用必须标 ok");
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
        let (profile, model, prompt, completion, cache_hit, _ok) = recorder.last();
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

    /// 失败不再是盲区：记一条 `ok = false` 的估算（输入按本地估算、输出 0）。
    #[tokio::test]
    async fn failed_chat_records_estimated_usage() {
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
        let mut req = request("m");
        req.messages = vec![echo_defs::message::ChatMessage::user(
            "这是一段足够长的中文输入，用来验证失败记录里的估算 token 不为零。",
        )];
        assert!(provider.chat(&req).await.is_err());
        assert_eq!(recorder.count(), 1, "失败必须留下一条记录");
        let (profile, model, prompt, completion, cache_hit, ok) = recorder.last();
        assert_eq!(profile, "p");
        assert_eq!(model, "m");
        assert!(!ok, "失败记录必须标 ok = false");
        assert!(prompt > 0, "输入 token 应给出非零估算值");
        assert_eq!((completion, cache_hit), (0, 0));
    }

    /// 取消（future 被 drop）也要记账：这是此前最大的盲区。
    #[tokio::test]
    async fn cancelled_chat_records_estimated_usage() {
        struct Hanging;
        #[async_trait]
        impl LlmProvider for Hanging {
            fn name(&self) -> &str {
                "hanging"
            }
            fn default_model(&self) -> &str {
                "hanging"
            }
            async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, LlmError> {
                std::future::pending::<()>().await;
                unreachable!()
            }
            async fn chat_stream(
                &self,
                _request: &ChatRequest,
                _tx: mpsc::UnboundedSender<ChatChunk>,
            ) -> Result<ChatResponse, LlmError> {
                std::future::pending::<()>().await;
                unreachable!()
            }
        }
        let recorder = Arc::new(RecordingRecorder::default());
        let provider = wrap_metering(Arc::new(Hanging), recorder.clone(), "p".into());
        let req = request("m");
        // 100ms 后放弃：future 被 drop，Drop guard 必须补记一条。
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            provider.chat(&req),
        )
        .await;
        assert_eq!(recorder.count(), 1, "取消必须留下一条记录");
        let (_, _, _, completion, _, ok) = recorder.last();
        assert!(!ok);
        assert_eq!(completion, 0);
    }
}
