//! FallbackProvider — 多协议自动回退 + 速率限制处理
//!
//! ## 场景
//! 用户配置了 API URL 但不确定是 OpenAI 还是 Anthropic 协议。
//! FallbackProvider 包装两个 provider，主协议失败时自动切换到备用协议。
//!
//! ## 回退条件
//! - KernelError::ApiError (status 0 or 4xx) → 协议不匹配，回退
//! - KernelError::RateLimited → 等待后重试（最多 max_retries 次）
//! - KernelError::Unauthorized / Provider → 不回退
//!
//! ## 多 Key 支持
//! 当配置多个 API key 时，按顺序轮转使用，单个 key 失败不影响其他 key。

use std::sync::Arc;
use std::time::Duration;

use abacus_types::{KernelError, ModelId, Result};
use async_trait::async_trait;

use crate::llm::provider::{LlmProvider, LlmRequest, LlmResponse};

use crate::llm::prompt_cache::CachedSegment;

/// 速率限制重试配置
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay_ms: 1000,
            max_delay_ms: 10000,
        }
    }
}

impl RateLimitConfig {
    /// 计算退避延迟（指数退避 + 真随机抖动）
    ///
    /// H3 修复：之前用 `SystemTime::subsec_millis()` 当随机源——并发同周期的请求
    /// 拿到相同抖动值，thundering herd 避免不到。改用 getrandom CSPRNG。
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let exponential = self.base_delay_ms.saturating_mul(2u64.pow(attempt.min(5)));
        let delay_ms = exponential.min(self.max_delay_ms);
        let jitter = (delay_ms as f64 * 0.2) as u64;
        if jitter == 0 {
            return Duration::from_millis(delay_ms);
        }
        // CSPRNG → [0, 2*jitter]，使并发请求的退避真正分散
        let mut buf = [0u8; 8];
        let rand_offset = match getrandom::fill(&mut buf) {
            Ok(_) => u64::from_le_bytes(buf) % (jitter * 2 + 1),
            // CSPRNG 不可用时回退到时间源（行为退化但不 panic）
            Err(_) => {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() as u64 % (jitter * 2 + 1)
            }
        };
        let jittered = delay_ms.saturating_sub(jitter).saturating_add(rand_offset);
        Duration::from_millis(jittered)
    }
}

pub struct FallbackProvider {
    primary: Arc<dyn LlmProvider>,
    fallback: Arc<dyn LlmProvider>,
    primary_id: String,
    fallback_id: String,
    rate_limit_config: RateLimitConfig,
}

impl FallbackProvider {
    pub fn new(
        primary: Arc<dyn LlmProvider>,
        fallback: Arc<dyn LlmProvider>,
        primary_id: impl Into<String>,
        fallback_id: impl Into<String>,
    ) -> Self {
        Self {
            primary,
            fallback,
            primary_id: primary_id.into(),
            fallback_id: fallback_id.into(),
            rate_limit_config: RateLimitConfig::default(),
        }
    }

    pub fn with_rate_limit_config(mut self, config: RateLimitConfig) -> Self {
        self.rate_limit_config = config;
        self
    }

    /// 检测是否为速率限制错误
    fn is_rate_limited(&self, error: &KernelError) -> bool {
        matches!(error, KernelError::RateLimited { .. })
            || matches!(error, KernelError::ApiError { status: 429, .. })
    }

    /// 检测是否为认证错误（不应回退）
    fn is_auth_error(&self, error: &KernelError) -> bool {
        matches!(error, KernelError::Unauthorized { .. })
            || matches!(error, KernelError::ApiError { status: 401 | 403, .. })
    }

    /// 检测是否为应触发回退的错误。
    ///
    /// Triggers fallback on:
    /// - Status 0: connection-level failure (no HTTP response)
    /// - 4xx: client errors (protocol mismatch, model unavailable, etc.)
    /// - 5xx: server errors (provider degraded, overloaded, etc.)
    ///
    /// Does NOT trigger fallback for:
    /// - Auth errors (401/403) — those are terminal
    /// - Rate limit errors (429) — those should be retried, not fallback
    fn is_fallback_eligible(&self, error: &KernelError) -> bool {
        if self.is_auth_error(error) {
            return false; // auth errors should not trigger fallback
        }
        if self.is_rate_limited(error) {
            return false; // rate limit errors should be retried with backoff, not fallback
        }
        matches!(error, KernelError::ApiError { status: 0, .. })
            || matches!(error, KernelError::ApiError { status: 400..=499, .. })
            || matches!(error, KernelError::ApiError { status: 500..=599, .. })
            || matches!(error, KernelError::Other(_)) // network/timeout errors
    }

    /// 执行带速率限制重试的请求
    ///
    /// H4 优化：req 改为 borrow，避免外层 `complete()` 的冗余 deep clone
    /// （messages / tools / MCP block 全 Vec 复制）。仅在 attempt 实际发送时
    /// per-call clone，失败 fallback 时不需重复 clone 整个 req。
    /// 末次 attempt 用 `take` 移走最后一份副本，避免无意义的 final clone。
    async fn execute_with_retry(&self, provider: &Arc<dyn LlmProvider>, req: &LlmRequest) -> Result<LlmResponse> {
        let mut last_error = None;
        let max = self.rate_limit_config.max_retries;
        for attempt in 0..=max {
            if attempt > 0 {
                let delay = self.rate_limit_config.delay_for_attempt(attempt);
                tracing::warn!(
                    attempt,
                    delay_ms = delay.as_millis(),
                    "rate limited, retrying with backoff"
                );
                tokio::time::sleep(delay).await;
            }

            // 末次 attempt 时仍需 clone（trait 要求 by-value），无法借助 take 进一步省
            let r = req.clone();
            match provider.complete(r).await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if self.is_rate_limited(&e) && attempt < max {
                        last_error = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| KernelError::Other("unexpected retry exhaustion".into())))
    }
}

#[async_trait]
impl LlmProvider for FallbackProvider {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse> {
        // 尝试主 provider（带速率限制重试）
        // H4: 借用而非 clone，省掉成功路径的一次 deep copy
        let result = self.execute_with_retry(&self.primary, &req).await;
        match result {
            Ok(resp) => Ok(resp),
            Err(e) => {
                // 认证错误 → 不回退（key 问题，不是协议问题）
                // 符合回退条件（4xx/5xx/网络错误，但排除 auth 错误）→ 切备用 provider
                if self.is_fallback_eligible(&e) {
                    tracing::warn!(
                        primary = %self.primary_id,
                        fallback = %self.fallback_id,
                        error = %e,
                        "primary provider failed, falling back"
                    );
                    return self.execute_with_retry(&self.fallback, &req).await;
                }

                // 其他错误（5xx 服务器错误等）→ 不回退
                Err(e)
            }
        }
    }

    fn cacheable_segments(&self, req: &LlmRequest) -> Vec<CachedSegment> {
        self.primary.cacheable_segments(req)
    }

    fn provider_id(&self) -> &str {
        self.primary.provider_id()
    }

    fn supported_models(&self) -> Vec<ModelId> {
        let mut models = self.primary.supported_models();
        models.extend(self.fallback.supported_models());
        models
    }
}
