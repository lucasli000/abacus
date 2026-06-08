//! ProviderRegistry — unified provider registration and model-aware routing.
//!
//! ## Purpose
//! Replaces scattered `providers: HashMap` + `provider_groups: Vec<ProviderGroup>` in CoreLoop
//! with a single registry that supports:
//! - Qualified model routing: `QualifiedModelId` → exact provider
//! - Priority-based disambiguation for unqualified model names
//! - Fallback provider (backward-compatible "primary" semantics)
//!
//! ## Design
//! Gradual migration facade: CoreLoop retains old fields during transition;
//! new code routes through ProviderRegistry first, falls back to old path if needed.
//! Thread-safe: internal `tokio::sync::RwLock`; callers only need `Arc<ProviderRegistry>`.
//!
//! ## References
//! - Created by: `CoreLoop::new()` (empty registry)
//! - Populated by: `CoreLoop::register_provider_group()` (dual-write to old + new)
//! - Consumed by: `CoreLoop::resolve_provider()` (primary routing path)
//! - Destroyed: with CoreLoop drop (Arc ref-count → 0)
//!
//! ## Lifecycle
//! - Construction: once per CoreLoop (session start)
//! - Mutation: only during provider registration (startup / hot-reload)
//! - Read: every turn (resolve_provider hot path — RwLock read, no contention)

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use abacus_types::{ModelId, ProviderId, QualifiedModelId};
use super::provider::LlmProvider;

/// A registered provider entry — binds a provider instance to the models it serves.
///
/// ## References
/// - Created by: `ProviderRegistry::register()`
/// - Consumed by: `ProviderRegistry::resolve()` (lookup by id or model scan)
/// - Destroyed: with ProviderRegistry drop
pub struct RegisteredProvider {
    /// Unique provider identifier (matches `CoreConfig.providers[].id`)
    pub id: ProviderId,
    /// Models this provider instance can serve
    pub models: Vec<ModelId>,
    /// The provider instance (shared via Arc for zero-copy routing)
    pub provider: Arc<dyn LlmProvider>,
    /// Priority for disambiguation (lower number = higher priority).
    /// When multiple providers serve the same model, lowest priority wins.
    pub priority: u32,
}

/// Unified provider registry with model-aware routing.
///
/// ## Thread safety
/// All interior state is behind `tokio::sync::RwLock` — safe for concurrent reads
/// (hot path) with infrequent writes (registration only at startup).
///
/// ## References
/// - Held by: `CoreLoop.provider_registry` as `Arc<ProviderRegistry>`
/// - Read by: `resolve_provider()` on every LLM request
/// - Written by: `register_provider_group()` during startup
pub struct ProviderRegistry {
    /// All registered providers (insertion order preserved for deterministic iteration)
    providers: RwLock<Vec<RegisteredProvider>>,
    /// Reverse index: model_name → [(provider_id, priority)], sorted by priority ascending
    model_index: RwLock<HashMap<String, Vec<(ProviderId, u32)>>>,
    /// Fallback provider — backward-compatible "primary" provider semantics.
    /// Used when no provider explicitly claims the requested model.
    fallback: RwLock<Option<Arc<dyn LlmProvider>>>,
}

impl ProviderRegistry {
    /// Create an empty registry. Providers are added via `register()`.
    pub fn new() -> Self {
        Self {
            providers: RwLock::new(Vec::new()),
            model_index: RwLock::new(HashMap::new()),
            fallback: RwLock::new(None),
        }
    }

    /// Register a provider with its supported models.
    ///
    /// Updates the reverse index for model → provider routing.
    /// If a model name already exists from another provider, both are retained
    /// and ordered by priority (lower = preferred).
    ///
    /// ## Parameters
    /// - `id`: unique provider identifier (e.g. "deepseek-prod", "anthropic")
    /// - `models`: model names this provider can serve
    /// - `provider`: the provider instance
    /// - `priority`: routing priority (lower = higher priority; 100 = default)
    pub async fn register(
        &self,
        id: impl Into<String>,
        models: Vec<String>,
        provider: Arc<dyn LlmProvider>,
        priority: u32,
    ) {
        let id = ProviderId(id.into());
        let model_ids: Vec<ModelId> = models.iter().map(|m| ModelId(m.clone())).collect();

        // Update reverse index
        {
            let mut index = self.model_index.write().await;
            for model in &models {
                let entry = index.entry(model.clone()).or_default();
                entry.push((id.clone(), priority));
                // Maintain sort by priority ascending (lowest first = highest priority)
                entry.sort_by_key(|(_, p)| *p);
            }
        }

        // Register provider entry
        self.providers.write().await.push(RegisteredProvider {
            id,
            models: model_ids,
            provider,
            priority,
        });
    }

    /// Set the fallback provider (backward-compatible "primary" semantics).
    ///
    /// When `resolve()` cannot find an explicit match via the reverse index,
    /// it falls back to this provider. This maintains compatibility with the
    /// old `providers.get("primary")` path.
    pub async fn set_fallback(&self, provider: Arc<dyn LlmProvider>) {
        *self.fallback.write().await = Some(provider);
    }

    /// Route a `QualifiedModelId` to a provider.
    ///
    /// ## Resolution order
    /// 1. **Qualified** (provider specified): exact match on provider id + model membership
    /// 2. **Unqualified** (provider=None): reverse index lookup, pick highest-priority provider
    /// 3. **Fallback**: if set, return as "primary" (backward compat)
    ///
    /// Returns `None` if no provider can serve the requested model.
    pub async fn resolve(&self, qid: &QualifiedModelId) -> Option<(ProviderId, Arc<dyn LlmProvider>)> {
        let providers = self.providers.read().await;

        // 1. Qualified: exact provider + model match
        if let Some(ref target_provider) = qid.provider {
            for rp in providers.iter() {
                if rp.id == *target_provider && rp.models.iter().any(|m| m == &qid.model) {
                    return Some((rp.id.clone(), rp.provider.clone()));
                }
            }
            // Provider specified but does not serve this model — do NOT fall through
            return None;
        }

        // 2. Unqualified: reverse index → best priority
        drop(providers); // release providers lock before acquiring index lock
        let index = self.model_index.read().await;
        if let Some(candidates) = index.get(&qid.model.0) {
            if let Some((best_provider_id, _)) = candidates.first() {
                // Re-acquire providers to get the actual Arc<dyn LlmProvider>
                let providers = self.providers.read().await;
                for rp in providers.iter() {
                    if rp.id == *best_provider_id {
                        return Some((rp.id.clone(), rp.provider.clone()));
                    }
                }
            }
        }
        drop(index);

        // 3. Fallback
        let fb = self.fallback.read().await;
        fb.as_ref().map(|p| (ProviderId("primary".into()), p.clone()))
    }

    /// Resolve an unqualified model name with ambiguity detection.
    ///
    /// Returns:
    /// - `Ok(QualifiedModelId)` if exactly one provider serves it, or if priorities break the tie
    /// - `Err(message)` if multiple providers at the same priority (user must qualify)
    /// - `Err(message)` if model is not found at all
    pub async fn resolve_unqualified(&self, model: &str) -> Result<QualifiedModelId, String> {
        let index = self.model_index.read().await;
        match index.get(model) {
            None => {
                // 提供更友好的错误信息
                let available_providers: Vec<&str> = index.values()
                    .flat_map(|candidates| candidates.iter().map(|(id, _)| id.0.as_str()))
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                if available_providers.is_empty() {
                    Err(format!(
                        "没有可用的 LLM Provider。请先配置 API Key：运行 `abacus config set llm.api_key <your-key>` 或设置环境变量 DEEPSEEK_API_KEY"
                    ))
                } else {
                    Err(format!(
                        "模型 '{}' 不可用。已注册的 Provider: [{}]。请检查模型名称是否正确，或使用 'provider:model' 格式指定",
                        model,
                        available_providers.join(", ")
                    ))
                }
            }
            Some(candidates) if candidates.len() == 1 => {
                Ok(QualifiedModelId {
                    provider: Some(candidates[0].0.clone()),
                    model: ModelId(model.to_string()),
                })
            }
            Some(candidates) => {
                // Multiple providers — check for priority tie at the top
                let (best_id, best_priority) = &candidates[0];
                let has_tie = candidates.len() > 1 && candidates[1].1 == *best_priority;
                if has_tie {
                    let tied_names: Vec<&str> = candidates.iter()
                        .filter(|(_, p)| *p == *best_priority)
                        .map(|(id, _)| id.0.as_str())
                        .collect();
                    Err(format!(
                        "模型 '{}' 在多个 Provider 中可用: [{}]。请使用 'provider:model' 格式指定，如 '{}:{}'",
                        model,
                        tied_names.join(", "),
                        tied_names[0],
                        model
                    ))
                } else {
                    // Clear winner by priority
                    Ok(QualifiedModelId {
                        provider: Some(best_id.clone()),
                        model: ModelId(model.to_string()),
                    })
                }
            }
        }
    }

    /// List all (provider, model) pairs across all registered providers.
    pub async fn list_all(&self) -> Vec<(ProviderId, ModelId)> {
        let providers = self.providers.read().await;
        let mut result = Vec::new();
        for rp in providers.iter() {
            for model in &rp.models {
                result.push((rp.id.clone(), model.clone()));
            }
        }
        result
    }

    /// Get a provider instance by its id.
    pub async fn get_provider(&self, id: &str) -> Option<Arc<dyn LlmProvider>> {
        let providers = self.providers.read().await;
        providers.iter()
            .find(|rp| rp.id.0 == id)
            .map(|rp| rp.provider.clone())
    }

    /// Check if a model name is ambiguous (multiple providers at the same top priority).
    ///
    /// Returns `false` if the model is not registered or has a clear priority winner.
    pub async fn is_ambiguous(&self, model: &str) -> bool {
        let index = self.model_index.read().await;
        match index.get(model) {
            Some(candidates) if candidates.len() >= 2 => {
                candidates[0].1 == candidates[1].1
            }
            _ => false,
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::Result;

    /// Minimal mock provider for testing routing logic
    struct MockProvider {
        id: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        async fn complete(&self, _req: crate::llm::LlmRequest) -> Result<crate::llm::LlmResponse> {
            unimplemented!("mock")
        }

        fn cacheable_segments(&self, _req: &crate::llm::LlmRequest) -> Vec<crate::llm::prompt_cache::CachedSegment> {
            vec![]
        }

        fn provider_id(&self) -> &str {
            &self.id
        }

        fn supported_models(&self) -> Vec<ModelId> {
            vec![]
        }
    }

    fn mock_provider(id: &str) -> Arc<dyn LlmProvider> {
        Arc::new(MockProvider { id: id.to_string() })
    }

    #[tokio::test]
    async fn test_resolve_qualified_exact() {
        let reg = ProviderRegistry::new();
        reg.register("anthropic", vec!["claude-opus-4-7".into()], mock_provider("anthropic"), 100).await;
        reg.register("deepseek", vec!["deepseek-v4-flash".into()], mock_provider("deepseek"), 100).await;

        // Qualified lookup
        let qid = QualifiedModelId::parse("anthropic:claude-opus-4-7");
        let result = reg.resolve(&qid).await;
        assert!(result.is_some());
        let (pid, _) = result.unwrap();
        assert_eq!(pid.0, "anthropic");
    }

    #[tokio::test]
    async fn test_resolve_qualified_wrong_model() {
        let reg = ProviderRegistry::new();
        reg.register("anthropic", vec!["claude-opus-4-7".into()], mock_provider("anthropic"), 100).await;

        // Ask anthropic for a model it doesn't have → None
        let qid = QualifiedModelId::parse("anthropic:gpt-5");
        let result = reg.resolve(&qid).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_resolve_unqualified_unique() {
        let reg = ProviderRegistry::new();
        reg.register("deepseek", vec!["deepseek-v4-flash".into()], mock_provider("deepseek"), 100).await;

        // Unqualified → only one provider has it
        let qid = QualifiedModelId::parse("deepseek-v4-flash");
        let result = reg.resolve(&qid).await;
        assert!(result.is_some());
        let (pid, _) = result.unwrap();
        assert_eq!(pid.0, "deepseek");
    }

    #[tokio::test]
    async fn test_resolve_unqualified_priority_wins() {
        let reg = ProviderRegistry::new();
        // deepseek-prod has higher priority (lower number)
        reg.register("deepseek-prod", vec!["deepseek-v4-flash".into()], mock_provider("deepseek-prod"), 50).await;
        reg.register("deepseek-staging", vec!["deepseek-v4-flash".into()], mock_provider("deepseek-staging"), 100).await;

        let qid = QualifiedModelId::parse("deepseek-v4-flash");
        let result = reg.resolve(&qid).await;
        assert!(result.is_some());
        let (pid, _) = result.unwrap();
        assert_eq!(pid.0, "deepseek-prod");
    }

    #[tokio::test]
    async fn test_resolve_unqualified_ambiguous_error() {
        let reg = ProviderRegistry::new();
        // Same priority → ambiguous
        reg.register("provider-a", vec!["shared-model".into()], mock_provider("provider-a"), 100).await;
        reg.register("provider-b", vec!["shared-model".into()], mock_provider("provider-b"), 100).await;

        let result = reg.resolve_unqualified("shared-model").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("多个 Provider") || err.contains("ambiguous"), "error should mention ambiguity: {}", err);
        assert!(err.contains("provider-a"));
        assert!(err.contains("provider-b"));
    }

    #[tokio::test]
    async fn test_resolve_fallback() {
        let reg = ProviderRegistry::new();
        reg.set_fallback(mock_provider("fallback")).await;

        // No providers registered for this model → fallback
        let qid = QualifiedModelId::parse("unknown-model");
        let result = reg.resolve(&qid).await;
        assert!(result.is_some());
        let (pid, _) = result.unwrap();
        assert_eq!(pid.0, "primary");
    }

    #[tokio::test]
    async fn test_resolve_no_fallback_returns_none() {
        let reg = ProviderRegistry::new();
        // No fallback set, no providers
        let qid = QualifiedModelId::parse("nonexistent");
        assert!(reg.resolve(&qid).await.is_none());
    }

    #[tokio::test]
    async fn test_is_ambiguous() {
        let reg = ProviderRegistry::new();
        reg.register("a", vec!["model-x".into()], mock_provider("a"), 100).await;
        reg.register("b", vec!["model-x".into()], mock_provider("b"), 100).await;
        reg.register("c", vec!["model-y".into()], mock_provider("c"), 50).await;
        reg.register("d", vec!["model-y".into()], mock_provider("d"), 100).await;

        assert!(reg.is_ambiguous("model-x").await); // same priority
        assert!(!reg.is_ambiguous("model-y").await); // different priority
        assert!(!reg.is_ambiguous("model-z").await); // not registered
    }

    #[tokio::test]
    async fn test_list_all() {
        let reg = ProviderRegistry::new();
        reg.register("anthropic", vec!["opus".into(), "sonnet".into()], mock_provider("anthropic"), 100).await;
        reg.register("deepseek", vec!["flash".into()], mock_provider("deepseek"), 100).await;

        let all = reg.list_all().await;
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn test_get_provider() {
        let reg = ProviderRegistry::new();
        reg.register("anthropic", vec!["opus".into()], mock_provider("anthropic"), 100).await;

        assert!(reg.get_provider("anthropic").await.is_some());
        assert!(reg.get_provider("nonexistent").await.is_none());
    }
}
