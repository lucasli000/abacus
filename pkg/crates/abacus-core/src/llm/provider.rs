use abacus_types::ModelId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use async_trait::async_trait;

use crate::llm::prompt_cache::{CachedSegment, PromptCacheConfig};
use abacus_types::{Result};

/// Chat message content — either a plain text string or a multi-part payload (vision, tools).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text.
    Text(String),
    /// Multi-modal parts (images, tool results, tool uses).
    MultiPart(Vec<ContentPart>),
}

/// A single part within a multi-part message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    /// Text content.
    #[serde(rename = "text")]
    Text { text: String },
    /// Image URL (base64 or URL reference).
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlSource },
    /// Result from a previous tool execution.
    #[serde(rename = "tool_result")]
    ToolResult { tool_use_id: String, content: String },
    /// A tool invocation request from the assistant.
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String, input: serde_json::Value },
}

/// Source reference for an image in a multi-part message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrlSource {
    /// Base64-encoded data URL or remote URL.
    pub url: String,
    /// Optional detail level ("auto", "low", "high").
    pub detail: Option<String>,
}

/// Chat message role
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A single message in the conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// DeepSeek-specific: reasoning content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// V31: DeepSeek prefix completion 标记
    ///
    /// ## 引用关系
    /// - 写：业务层（如 cancel-resume / Tool JSON 强约束）在 LlmRequest.messages 末尾
    ///   插入 {role: Assistant, content: "<前缀>", prefix: true} 时设
    /// - 读：DeepSeek provider build_request 检测此字段，序列化时附加 "prefix": true 字段
    /// - 其他 provider (Anthropic/OpenAI/Gemini) 忽略（默认序列化不含此字段，向后兼容）
    ///
    /// ## 生命周期
    /// - 仅 messages 列表最后一条 assistant message 可设 true（DeepSeek API 约束）
    /// - 配合 base_url=.../beta 才生效（model.supports_prefix_completion=true 时启用 beta）
    ///
    /// ## 默认值
    /// - false（普通消息）；序列化时若为 false 则跳过（保持现有 wire 格式）
    #[serde(default, skip_serializing_if = "is_false")]
    pub prefix: bool,
}

/// 序列化辅助：prefix=false 时跳过字段输出（保持 wire 格式向后兼容）
fn is_false(b: &bool) -> bool {
    !b
}

/// Tool call within a message (response-side).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique identifier for this tool call.
    pub id: String,
    /// Always "function".
    #[serde(rename = "type")]
    pub type_: String,
    /// The function to invoke.
    pub function: ToolFunction,
}

/// Function attributes within a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    /// The function name.
    pub name: String,
    /// JSON-encoded function arguments.
    pub arguments: String,
}

/// Tool definition for function calling (sent in the request).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub type_: String,
    pub function: ToolFunctionSpec,
}

/// Specification of a tool's function signature (request-side).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunctionSpec {
    /// The function name (must match regex: `^[a-zA-Z_][a-zA-Z0-9_-]{2,63}$`).
    pub name: String,
    /// Human-readable description of what the function does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the parameters.
    pub parameters: serde_json::Value,
    /// Whether to enforce strict schema validation (provider-dependent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// A segment of the system prompt with cacheability metadata.
///
/// Providers that support multi-block system prompts (e.g., Anthropic) can use
/// these segments to apply per-block cache_control headers. Providers without
/// multi-block support concatenate all segments into a single string.
#[derive(Debug, Clone)]
pub struct SystemSegment {
    /// The text content of this segment
    pub text: String,
    /// Whether this segment is stable across turns and should be cached.
    /// Stable segments (kernel, strategy) get cache_control markers.
    pub cacheable: bool,
}

/// A request to an LLM provider
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: ModelId,
    pub messages: Vec<Message>,
    /// Legacy single-string system prompt (used when system_segments is empty)
    pub system: Option<String>,
    /// Multi-segment system prompt with per-segment cache hints.
    /// When non-empty, providers should prefer this over `system`.
    /// Segments are ordered from highest priority (stable) to lowest (dynamic).
    pub system_segments: Vec<SystemSegment>,
    pub tools: Vec<ToolDefinition>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f64>,
    pub stop: Vec<String>,
    pub stream: bool,
    /// Thinking 意图层（L1 删除旧 ThinkingConfig 后唯一通道）。
    ///
    /// ## 引用关系
    /// 写入：pipeline 在 setup() 阶段从 CoreConfig.thinking_intent / SessionState 决议结果填充
    /// 读取：4 家 provider 的 build_request 内部 resolve_thinking 路由到各自 native enum
    pub thinking_intent: Option<abacus_types::ThinkingIntent>,
    pub cache_config: Option<PromptCacheConfig>,
    pub extra_body: HashMap<String, serde_json::Value>,

    /// Phase 4 KV cache 修复：注入到 latest user message 顶部的动态 preamble。
    ///
    /// ## 引用关系
    /// 写入：pipeline 把 ICL Primer / 临时 RAG 结果等"本轮检索素材"写到这里，而非 push_dynamic
    /// 读取：provider 在 build_messages 时把 preamble 拼到最后一条 user message 的 content 顶部
    ///
    /// ## KV cache 设计
    /// 之前 ICL 等内容 push_dynamic 到 system → 每轮 byte 变化破坏前缀。改放到 latest user message
    /// 后，system+history 全 stable，只有 last user message（永不缓存）携带 dynamic preamble，
    /// 不破 cache 前缀。
    ///
    /// ## 语义
    /// 通常 framing 为 `## Relevant KB Context` / `## Retrieved Materials` 等，LLM 视为本轮检索素材。
    /// None → 无注入（向后兼容）。
    pub user_message_preamble: Option<String>,
}

// L1 已删除：`ThinkingType` enum + `ThinkingConfig` struct + `LlmRequest.thinking` 字段
// 用 `ThinkingIntent` (abacus-types::ThinkingIntent) 单通道替代。
// 删除日期：2026-05-23（major bump 配套）。

/// A response from an LLM provider
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub model: ModelId,
    pub message: Message,
    pub finish_reason: String,
    pub usage: TokenUsage,
    pub thinking: Option<String>,
    pub cache_stats: Option<CacheStats>,
}

/// Token consumption for a single LLM response.
///
/// V30 语义规范化不变量（跨 4 家 provider 必须一致满足）：
/// 1. `prompt_tokens` 是该轮输入总量，含 cached_tokens 与 cache_creation_tokens 子集。
///    Anthropic API 拆分 input/cache_read/cache_creation 三段字段，provider 层必须合并
///    后填入 prompt_tokens（详 anthropic.rs:712）。
/// 2. `cached_tokens` 是 prompt_tokens 的子集（OpenAI 语义）。
/// 3. `completion_tokens` 是输出 tokens 总量，含 thinking_tokens 子集。
/// 4. `thinking_tokens` 是 completion_tokens 的子集——包含在 completion 里作费用计算，
///    单独曝露仅供透明度用途（TUI 面板显示"思考 X"行）。
/// 5. `total_tokens == prompt_tokens + completion_tokens`。以前有量路径信任 API 返回
///    的 total，但跨 thinking 模式 / 多 provider 不一致，V30 后统一 manual sum。
///
/// 引用关系：
/// - 填值端：4 家 provider 的响应解析代码。
/// - 读值端：TurnStats 镜像 TokenUsage 同名字段 → SessionTokenStats 累加 →
///   render_tab_memory 渲染 / cost 计算
#[derive(Debug, Clone, Copy)]
pub struct TokenUsage {
    /// 该轮输入总量，含 cached + cache_creation
    pub prompt_tokens: u64,
    /// 该轮输出总量，含 thinking_tokens
    pub completion_tokens: u64,
    /// V30: 严格等于 prompt_tokens + completion_tokens
    pub total_tokens: u64,
    /// 命中提供商缓存的 input tokens（prompt_tokens 的子集）
    pub cached_tokens: u64,
    /// 创建缓存所消耗的 input tokens（provider-依赖，跨 Anthropic：prompt_tokens 的子集）
    pub cache_creation_tokens: u64,
    /// V30: 推理/思考 tokens（completion_tokens 的子集）。Anthropic 无独立字段返 0；
    /// DeepSeek/OpenAI 从 completion_tokens_details.reasoning_tokens 提取；
    /// Gemini 从 thoughtsTokenCount 提取。
    pub thinking_tokens: u64,
}

/// Provider-level cache statistics for cost tracking.
#[derive(Debug, Clone, Copy)]
pub struct CacheStats {
    /// Tokens consumed to create/prolong cache entries.
    pub cache_creation_tokens: u64,
    /// Tokens served from cache (free or discounted).
    pub cache_read_tokens: u64,
}

/// Abstraction over LLM providers.
///
/// Each provider implements `complete()` for non-streaming inference and
/// `cacheable_segments()` to inform the caller which request parts are cacheable.
///
/// V0.2: `stream_complete()` adds streaming support with a default fallback
/// that calls `complete()` and emits the full response as a single event.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Send a complete request and get a response.
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse>;

    /// P2: 取消感知 complete。default impl 用 `tokio::select!` 让 `complete()`
    /// 与 `token.cancelled()` 竞速：取消时 future 被 drop，reqwest 自动中止 HTTP
    /// 请求（tokio runtime 保证）。
    ///
    /// Provider 不需要重写——drop 即取消的语义对所有 reqwest-based provider 生效。
    /// 若 provider 用其他网络栈（如自定义 transport），可重写以接通本地 cancel。
    async fn complete_cancellable(
        &self,
        req: LlmRequest,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<LlmResponse> {
        match cancel {
            Some(token) => tokio::select! {
                resp = self.complete(req) => resp,
                _ = token.cancelled() => Err(abacus_types::KernelError::Other(
                    "request cancelled".into()
                )),
            },
            None => self.complete(req).await,
        }
    }

    /// V0.2: Stream completion — delivers incremental text via channel.
    ///
    /// Default implementation falls back to `complete()` + single TextDelta.
    /// Providers that support SSE should override this for real streaming.
    ///
    /// Returns the final aggregated LlmResponse (same as complete()).
    async fn stream_complete(
        &self,
        req: LlmRequest,
        tx: tokio::sync::mpsc::UnboundedSender<super::stream::StreamEvent>,
    ) -> Result<LlmResponse> {
        let resp = self.complete(req).await?;
        // Extract text content for the delta event
        let text = match &resp.message.content {
            Some(MessageContent::Text(t)) => t.clone(),
            _ => String::new(),
        };
        if !text.is_empty() {
            let _ = tx.send(super::stream::StreamEvent::TextDelta(text));
        }
        let _ = tx.send(super::stream::StreamEvent::Usage {
            prompt_tokens: resp.usage.prompt_tokens,
            completion_tokens: resp.usage.completion_tokens,
        });
        let _ = tx.send(super::stream::StreamEvent::Done);
        Ok(resp)
    }

    /// Identify which segments of a request can be cached by this provider,
    /// along with breakpoint markers.
    fn cacheable_segments(&self, req: &LlmRequest) -> Vec<CachedSegment>;

    /// Provider identifier (e.g. "deepseek")
    fn provider_id(&self) -> &str;

    /// Models this provider can handle (静态硬编码列表)
    fn supported_models(&self) -> Vec<ModelId>;

    /// 通过 provider API 动态发现可用模型（首次配置自动拉取）
    ///
    /// ## 默认实现
    /// 回退到 `supported_models()` 静态列表 — 不打网络。
    /// 支持 `/v1/models` 端点的 provider（OpenAI-compatible / DeepSeek）应当重写。
    ///
    /// ## 错误处理
    /// 网络/解析失败返回 `Err`，调用方应当 fallback 到 `supported_models()`。
    /// 不应让 discover 失败导致整个启动流程失败。
    async fn discover_models(&self) -> Result<Vec<ModelId>> {
        Ok(self.supported_models())
    }
}