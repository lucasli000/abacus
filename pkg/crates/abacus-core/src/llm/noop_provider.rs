//! Shared no-op LLM provider — used as fallback when no API key is configured.
//!
//! Returns a descriptive error message so users know to configure credentials.

use crate::llm::{
    LlmProvider, LlmRequest, LlmResponse, Message, MessageContent, MessageRole, TokenUsage,
};
use crate::llm::prompt_cache::CachedSegment;
use abacus_types::ModelId;

/// No-op LLM provider that returns a helpful error message.
///
/// Registered when `ABACUS_API_KEY` / `DEEPSEEK_API_KEY` is not set.
/// This allows the engine to initialize and respond gracefully rather than crashing.
pub struct NoApiKeyProvider;

#[async_trait::async_trait]
impl LlmProvider for NoApiKeyProvider {
    async fn complete(&self, _req: LlmRequest) -> abacus_types::Result<LlmResponse> {
        Ok(LlmResponse {
            model: ModelId("none".into()),
            message: Message {
                role: MessageRole::Assistant,
                content: Some(MessageContent::Text(
                    "[Error: No API key configured. Set DEEPSEEK_API_KEY or ABACUS_API_KEY]"
                        .into(),
                )),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                prefix: false,
            },
            finish_reason: "stop".into(),
            usage: TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                cached_tokens: 0,
                cache_creation_tokens: 0,
                thinking_tokens: 0,
            },
            thinking: None,
            cache_stats: None,
        })
    }

    fn cacheable_segments(&self, _req: &LlmRequest) -> Vec<CachedSegment> {
        Vec::new()
    }

    fn provider_id(&self) -> &str {
        "no-api-key"
    }

    fn supported_models(&self) -> Vec<ModelId> {
        vec![ModelId("none".into())]
    }
}
