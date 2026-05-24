/// Segments of a request that can be cached by the provider.
#[derive(Debug, Clone)]
pub struct CachedSegment {
    pub kind: CachedSegmentKind,
    pub content: String,
    pub breakpoint_after: bool,
}

/// Category of a cacheable segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachedSegmentKind {
    SystemPrompt,
    ToolDefinitions,
    KnowledgeContext,
    ConversationPrefix,
}

/// Provider-side caching configuration.
#[derive(Debug, Clone)]
pub struct PromptCacheConfig {
    pub enabled: bool,
    pub min_cacheable_tokens: u64,
    pub breakpoints: Vec<CacheBreakpoint>,
}

impl Default for PromptCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_cacheable_tokens: 1024,
            breakpoints: vec![
                CacheBreakpoint::AfterSystemPrompt,
                CacheBreakpoint::AfterToolDefinitions,
            ],
        }
    }
}

/// Where to place cache breakpoints.
#[derive(Debug, Clone)]
pub enum CacheBreakpoint {
    AfterSystemPrompt,
    AfterToolDefinitions,
    AfterKnowledge,
    AfterConversationPrefix,
}

/// Provider-agnostic cache hit statistics.
#[derive(Debug, Clone, Copy)]
pub struct PromptCacheStats {
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_hit_rate: f64,
}