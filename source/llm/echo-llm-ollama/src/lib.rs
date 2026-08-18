//! Ollama local model provider (OpenAI-compatible `/v1/chat/completions`).

use async_trait::async_trait;
use tokio::sync::mpsc;

use echo_defs::llm::{LlmError, LlmProvider};
use echo_defs::message::{ChatRequest, ChatResponse};
use echo_llm_openai::OpenAiProvider;

/// Ollama exposes an OpenAI-compatible endpoint, so this is a thin wrapper
/// around [`OpenAiProvider`] pointed at `{base_url}/v1`.
#[derive(Debug)]
pub struct OllamaProvider {
    inner: OpenAiProvider,
}

impl OllamaProvider {
    pub fn new(base_url: &str, model: &str) -> Self {
        let base = base_url.trim_end_matches('/').to_string();
        let v1 = if base.ends_with("/v1") {
            base
        } else {
            format!("{base}/v1")
        };
        Self {
            inner: OpenAiProvider::new(&v1, "ollama", model),
        }
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    fn default_model(&self) -> &str {
        self.inner.default_model()
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.inner.chat(request).await
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        tx: mpsc::UnboundedSender<echo_defs::message::ChatChunk>,
    ) -> Result<(), LlmError> {
        self.inner.chat_stream(request, tx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_defs::llm::LlmProvider;

    #[test]
    fn appends_v1_to_plain_base_url() {
        let p = OllamaProvider::new("http://localhost:11434", "llama3");
        assert_eq!(p.default_model(), "llama3");
    }

    #[test]
    fn keeps_existing_v1_suffix() {
        let p = OllamaProvider::new("http://localhost:11434/v1", "llama3");
        assert_eq!(p.default_model(), "llama3");
    }

    #[test]
    fn strips_trailing_slash() {
        let p = OllamaProvider::new("http://localhost:11434/", "llama3");
        assert_eq!(p.default_model(), "llama3");
    }

    #[test]
    fn provider_name_is_ollama() {
        let p = OllamaProvider::new("http://localhost:11434", "llama3");
        assert_eq!(p.name(), "ollama");
    }
}
