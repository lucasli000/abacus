//! Phase Stability-1：RetryProvider —— 给任意 [`LlmProvider`] 装上指数退避重试
//!
//! ## 设计
//! 用 `backon::ExponentialBuilder` 生成 backoff 策略；只对**可重试错误**触发：
//! - `KernelError::ApiError { status: 429 | 500..=599, .. }` —— 服务端瞬态
//! - `KernelError::Provider(_)` —— 网络/连接错误
//! - `KernelError::RateLimited { .. }` —— provider 显式限流（注意 retry_after 已在内部）
//!
//! 不重试：
//! - `Unauthorized` / `ModelNotFound` / `ContextOverflow` / `Validation` —— 客户端永久错
//! - `OutputAborted` / cancellation —— 用户主动
//!
//! ## 与 FallbackProvider 的关系
//! - `RetryProvider` —— 单 provider 的瞬态错误重试
//! - `FallbackProvider` —— 主 provider 持续失败时切换到备 provider
//! 两者正交可叠加（`FallbackProvider::new(RetryProvider::new(p1), RetryProvider::new(p2))`）。
//!
//! ## 引用关系
//! - 上游：用户在 `register_provider` 时按需用 RetryProvider 包装（opt-in）
//! - 下游：`backon` crate
//!
//! 默认 **不自动启用**——遵循 default-off 原则保持现有 KV cache + 行为稳定。

use std::sync::Arc;
use std::time::Duration;

use abacus_types::{KernelError, ModelId, Result};
use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};

use super::prompt_cache::CachedSegment;
use super::provider::{LlmProvider, LlmRequest, LlmResponse};

/// 判断某 KernelError 是否值得重试
fn is_retriable(err: &KernelError) -> bool {
    match err {
        KernelError::ApiError { status, .. } => {
            *status == 429 || (500..=599).contains(status)
        }
        KernelError::Provider(_) => true,
        KernelError::RateLimited { .. } => true,
        // 客户端永久错——不重试
        KernelError::Unauthorized(_)
        | KernelError::ModelNotFound(_)
        | KernelError::ContextOverflow { .. }
        | KernelError::Validation(_)
        | KernelError::Config(_)
        | KernelError::OutputAborted(_) => false,
        // 其余保守不重试（业务语义错误重试无益）
        _ => false,
    }
}

/// RetryProvider 配置
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// 最大重试次数（不含首次尝试）。默认 3。
    pub max_retries: usize,
    /// 初始等待时间。默认 200ms。
    pub initial_delay: Duration,
    /// 最大等待时间——每次重试 delay 不会超过此值。默认 5s。
    pub max_delay: Duration,
    /// 指数因子。默认 2.0（200ms → 400ms → 800ms → 1.6s）。
    pub factor: f32,
    /// 是否加入 jitter（避免 thundering herd）。默认 true。
    pub jitter: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(5),
            factor: 2.0,
            jitter: true,
        }
    }
}

impl RetryConfig {
    fn to_builder(&self) -> ExponentialBuilder {
        let mut b = ExponentialBuilder::default()
            .with_min_delay(self.initial_delay)
            .with_max_delay(self.max_delay)
            .with_factor(self.factor)
            .with_max_times(self.max_retries);
        if self.jitter {
            b = b.with_jitter();
        }
        b
    }
}

/// 装饰器：把任意 LlmProvider 包成"瞬态错误自动重试"版本
pub struct RetryProvider {
    inner: Arc<dyn LlmProvider>,
    config: RetryConfig,
}

impl RetryProvider {
    pub fn new(inner: Arc<dyn LlmProvider>) -> Self {
        Self {
            inner,
            config: RetryConfig::default(),
        }
    }

    pub fn with_config(inner: Arc<dyn LlmProvider>, config: RetryConfig) -> Self {
        Self { inner, config }
    }
}

#[async_trait]
impl LlmProvider for RetryProvider {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse> {
        let inner = self.inner.clone();
        let req_arc = Arc::new(req);
        let backoff = self.config.to_builder();

        // backon 的 Retryable 接受 FnMut() -> Future。我们克隆 Arc 保证可重试。
        let req_for_closure = req_arc.clone();
        let result = (move || {
            let inner = inner.clone();
            let req = (*req_for_closure).clone();
            async move { inner.complete(req).await }
        })
        .retry(backoff)
        .when(is_retriable)
        .notify(|err, dur: Duration| {
            tracing::warn!(
                error = %err,
                retry_after_ms = dur.as_millis() as u64,
                "RetryProvider: retriable error, backing off"
            );
        })
        .await;

        result
    }

    fn cacheable_segments(&self, req: &LlmRequest) -> Vec<CachedSegment> {
        self.inner.cacheable_segments(req)
    }

    fn provider_id(&self) -> &str {
        self.inner.provider_id()
    }

    fn supported_models(&self) -> Vec<ModelId> {
        self.inner.supported_models()
    }

    async fn discover_models(&self) -> Result<Vec<ModelId>> {
        self.inner.discover_models().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::provider::{Message, MessageRole, TokenUsage};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 测试 provider：前 N 次调用返回指定错误，第 N+1 次返回成功
    struct FlakyProvider {
        fail_count: AtomicU32,
        fail_until: u32,
        fail_with: KernelError,
    }

    impl FlakyProvider {
        fn new(fail_until: u32, fail_with: KernelError) -> Self {
            Self {
                fail_count: AtomicU32::new(0),
                fail_until,
                fail_with,
            }
        }
        fn calls(&self) -> u32 {
            self.fail_count.load(Ordering::SeqCst)
        }
    }

    impl Clone for FlakyProvider {
        fn clone(&self) -> Self {
            Self {
                fail_count: AtomicU32::new(self.fail_count.load(Ordering::SeqCst)),
                fail_until: self.fail_until,
                fail_with: clone_kernel_error(&self.fail_with),
            }
        }
    }

    fn clone_kernel_error(e: &KernelError) -> KernelError {
        match e {
            KernelError::ApiError { status, body } => KernelError::ApiError { status: *status, body: body.clone() },
            KernelError::Provider(s) => KernelError::Provider(s.clone()),
            KernelError::Unauthorized(s) => KernelError::Unauthorized(s.clone()),
            KernelError::RateLimited { retry_after } => KernelError::RateLimited { retry_after: *retry_after },
            _ => KernelError::Other("clone fallback".into()),
        }
    }

    #[async_trait]
    impl LlmProvider for FlakyProvider {
        async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse> {
            let n = self.fail_count.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_until {
                return Err(clone_kernel_error(&self.fail_with));
            }
            Ok(LlmResponse {
                model: ModelId("flaky".into()),
                message: Message {
                    role: MessageRole::Assistant,
                    content: Some(crate::llm::provider::MessageContent::Text("ok".into())),
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
            vec![]
        }

        fn provider_id(&self) -> &str {
            "flaky"
        }

        fn supported_models(&self) -> Vec<ModelId> {
            vec![ModelId("flaky".into())]
        }
    }

    fn mk_req() -> LlmRequest {
        LlmRequest {
            model: ModelId("flaky".into()),
            messages: vec![],
            system: None,
            system_segments: vec![],
            tools: vec![],
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: vec![],
            stream: false,
            thinking_intent: None,
            cache_config: None,
            extra_body: std::collections::HashMap::new(),
            user_message_preamble: None,
        }
    }

    fn fast_config() -> RetryConfig {
        RetryConfig {
            max_retries: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            factor: 2.0,
            jitter: false,
        }
    }

    #[tokio::test]
    async fn retries_on_5xx_then_succeeds() {
        let inner = Arc::new(FlakyProvider::new(2, KernelError::ApiError { status: 503, body: "down".into() }));
        let calls_ref = inner.clone();
        let p = RetryProvider::with_config(inner, fast_config());
        let resp = p.complete(mk_req()).await;
        assert!(resp.is_ok(), "应该最终成功");
        assert_eq!(calls_ref.calls(), 3, "前 2 次失败 + 第 3 次成功");
    }

    #[tokio::test]
    async fn retries_on_provider_network_error() {
        let inner = Arc::new(FlakyProvider::new(1, KernelError::Provider("connection reset".into())));
        let calls_ref = inner.clone();
        let p = RetryProvider::with_config(inner, fast_config());
        let _ = p.complete(mk_req()).await;
        assert!(calls_ref.calls() >= 2, "至少应重试 1 次");
    }

    #[tokio::test]
    async fn does_not_retry_on_4xx_unauthorized() {
        let inner = Arc::new(FlakyProvider::new(99, KernelError::Unauthorized("bad key".into())));
        let calls_ref = inner.clone();
        let p = RetryProvider::with_config(inner, fast_config());
        let resp = p.complete(mk_req()).await;
        assert!(resp.is_err());
        assert_eq!(calls_ref.calls(), 1, "Unauthorized 不应重试");
    }

    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let inner = Arc::new(FlakyProvider::new(99, KernelError::ApiError { status: 503, body: "down".into() }));
        let calls_ref = inner.clone();
        let p = RetryProvider::with_config(inner, fast_config());
        let resp = p.complete(mk_req()).await;
        assert!(resp.is_err());
        // 1 次首发 + max_retries 次重试 = 4
        assert_eq!(calls_ref.calls(), 4);
    }

    #[test]
    fn is_retriable_classifies_errors_correctly() {
        assert!(is_retriable(&KernelError::ApiError { status: 503, body: "".into() }));
        assert!(is_retriable(&KernelError::ApiError { status: 429, body: "".into() }));
        assert!(is_retriable(&KernelError::Provider("net".into())));
        assert!(is_retriable(&KernelError::RateLimited { retry_after: 5 }));

        assert!(!is_retriable(&KernelError::Unauthorized("".into())));
        assert!(!is_retriable(&KernelError::ApiError { status: 400, body: "".into() }));
        assert!(!is_retriable(&KernelError::ModelNotFound("x".into())));
        assert!(!is_retriable(&KernelError::OutputAborted("user".into())));
    }
}
