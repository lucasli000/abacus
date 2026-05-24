//! OpenAI-compatible provider — 通用 /v1/chat/completions 标准
//!
//! ## 适用
//! 任何实现 OpenAI Chat Completions API 标准的服务：
//! - OpenRouter / Together AI / Groq / Fireworks
//! - Azure OpenAI / GitHub Models
//! - 本地 vLLM / Ollama / llama.cpp 等
//!
//! ## 配置
//! 通过 ConfigManager 的 `llm.*` 或环境变量 `ABACUS_LLM__*` 驱动：
//! - `llm.openai_base_url`: API 端点（必填）
//! - `llm.openai_api_key`: API 密钥
//! - `llm.openai_model`: 模型名（默认取 `core.default_model`）
//! - `llm.openai_auth_header`: 认证头名（默认 `Authorization`）
//! - `llm.openai_auth_prefix`: 认证前缀（默认 `Bearer `）

use abacus_types::{KernelError, ModelId, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::debug;

use crate::llm::prompt_cache::{CachedSegment, CachedSegmentKind};
use crate::llm::provider::{
    LlmProvider, LlmRequest, LlmResponse, Message, MessageContent, MessageRole,
    TokenUsage,
};

struct SecretString(zeroize::Zeroizing<String>);

impl SecretString {
    fn new(s: String) -> Self { Self(zeroize::Zeroizing::new(s)) }
    fn as_str(&self) -> &str { &self.0 }
}

/// L1 后单通道：解析 reasoning_effort 字符串。
/// 唯一来源：req.thinking_intent。
///
/// ## OpenAI 接受的档位字符串
/// - `minimal`（GPT-5 系列专属）
/// - `low` / `medium` / `high`（o1/o3/o4-mini/GPT-5 通用）
///
/// ## EffortLevel 映射策略
/// - `Minimal` → `"minimal"`
/// - `Low` / `Medium` / `High` → 同名字符串
/// - `Max` / `XHigh` → 降级为 `"high"`（OpenAI 无更高档位）
/// - `Adaptive` / `Off` → None
/// - `Budget(_)` → None（OpenAI 不接受 budget 字段，丢弃）
fn resolve_openai_reasoning_effort(req: &LlmRequest) -> Option<String> {
    use abacus_types::{EffortLevel, ThinkingIntent};

    let intent = req.thinking_intent.as_ref()?;
    match intent {
        ThinkingIntent::Off | ThinkingIntent::Budget(_) => None,
        ThinkingIntent::Adaptive => None,
        ThinkingIntent::Effort(level) => Some(match level {
            EffortLevel::Minimal => "minimal".into(),
            EffortLevel::Low => "low".into(),
            EffortLevel::Medium => "medium".into(),
            EffortLevel::High => "high".into(),
            EffortLevel::Max | EffortLevel::XHigh => "high".into(),
        }),
    }
}

// ── 标准 OpenAI 请求/响应类型 ─────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDef>>,
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct ToolDef {
    #[serde(rename = "type")]
    type_: String,
    function: ToolFunction,
}

#[derive(Serialize)]
struct ToolFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

#[derive(Serialize, Deserialize)]
struct ToolCall {
    id: String,
    #[serde(rename = "type")]
    type_: String,
    function: ToolCallFunction,
}

#[derive(Serialize, Deserialize)]
struct ToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    model: String,
}

#[derive(Deserialize)]
struct Choice {
    #[allow(dead_code)]
    index: u32,
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: String,
}

#[derive(Deserialize)]
struct ResponseMessage {
    #[allow(dead_code)]
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

/// OpenAI-compatible 请求缓存统计（prompt_tokens_details 子字段）
///
/// ## 支持该格式的服务商
/// - OpenAI GPT-4o 系列：自动前缀缓存，50% 折扣
/// - Qwen/Dashscope：OpenAI-compatible API，返回同格式
/// - Moonshot/Kimi：OpenAI-compatible API，返回同格式
/// - SiliconFlow：中继底层模型（DeepSeek/Qwen）的缓存统计
///
/// OpenAI-compatible 缓存详情子字段
///
/// ## 各家字段实战对比
/// | 服务商      | cached_tokens | cache_creation_input_tokens | 说明 |
/// |------------|---------------|-----------------------------|---------|
/// | OpenAI     | 命中 token 数 | 无此字段                  | 自动 50% |
/// | DeepSeek   | 命中 token 数 | 无此字段                  | 自动 90% |
/// | GLM-4.7    | 命中 token 数 | 无此字段                  | 自动 82% |
/// | Qwen 隐式  | 命中 token 数 | 无此字段                  | 自动 80% |
/// | Qwen 显式  | 命中 token 数 | 创建缓存的 token 数         | 需 cache_control |
/// | MiniMax    | 命中 token 数 | 无此字段                  | 自动 |
/// | xAI/Grok   | 命中 token 数 | 无此字段                  | 自动 |
#[derive(Debug, Deserialize, Default)]
struct PromptTokensDetails {
    /// KV 缓存命中的 token 数（所有实现均支持）
    #[serde(default)]
    cached_tokens: u64,
    /// 创建缓存时消耗的 token（Qwen 显式缓存返回，其他平台为 0）
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    /// V30 后改为 manual sum；字段保留供未来诊断比对
    #[serde(default)]
    #[allow(dead_code)]
    total_tokens: u64,
    /// KV 缓存命中统计（OpenAI/Qwen/Moonshot/SiliconFlow 均使用此字段）
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// V30：OpenAI o-series / GPT-5 reasoning_tokens（completion 子集）
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Deserialize, Default)]
struct CompletionTokensDetails {
    /// 推理 tokens（completion_tokens 子集，仅曝露用）
    #[serde(default)]
    reasoning_tokens: u64,
}

// ── Provider ───────────────────────────────────────────────────────────

/// 通用 OpenAI-compatible Chat Completions provider
///
/// 通过配置支持任何兼容 OpenAI API 的服务。
/// 与 DeepSeekProvider 的区别：无硬编码定价、base_url 必填、支持自定义认证头。
pub struct OpenAICompatibleProvider {
    client: Client,
    /// H8: 每请求 timeout（共享 Client 池 + per-request timeout 配置）
    request_timeout: Duration,
    api_key: SecretString,
    base_url: String,
    model: ModelId,
    auth_header: String,
    auth_prefix: String,
    default_max_tokens: u32,
}

impl OpenAICompatibleProvider {
    /// 创建通用 OpenAI-compatible provider
    ///
    /// # Arguments
    /// * `api_key` - API 密钥
    /// * `model` - 模型名
    /// * `base_url` - API 基础 URL（例如 `https://api.openai.com`，不含 `/v1/...`）
    /// * `auth_header` - 认证头名（默认 `Authorization`）
    /// * `auth_prefix` - 认证前缀（默认 `Bearer `，注意尾部空格）
    /// * `timeout_secs` - 请求超时秒数（默认 120）
    pub fn new(
        api_key: String,
        model: impl Into<ModelId>,
        base_url: String,
        auth_header: Option<String>,
        auth_prefix: Option<String>,
        timeout_secs: Option<u64>,
    ) -> Self {
        // H8: 复用进程级共享 Client
        let client = crate::llm::shared_http_client().clone();
        let request_timeout = Duration::from_secs(timeout_secs.unwrap_or(120));

        Self {
            client,
            request_timeout,
            api_key: SecretString::new(api_key),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.into(),
            auth_header: auth_header.unwrap_or_else(|| "Authorization".into()),
            auth_prefix: auth_prefix.unwrap_or_else(|| "Bearer ".into()),
            default_max_tokens: 32000,
        }
    }

    fn build_request(&self, req: &LlmRequest) -> ChatRequest {
        let messages: Vec<ChatMessage> = self.build_messages(req);

        let tools = if req.tools.is_empty() {
            None
        } else {
            Some(
                req.tools
                    .iter()
                    .map(|t| ToolDef {
                        type_: "function".into(),
                        function: ToolFunction {
                            name: t.function.name.clone(),
                            description: t.function.description.clone(),
                            parameters: t.function.parameters.clone(),
                            strict: t.function.strict,
                        },
                    })
                    .collect(),
            )
        };

        // Phase 2：优先 thinking_intent（含 Minimal/Max/XHigh），fallback 旧 thinking 字段
        let reasoning_effort = resolve_openai_reasoning_effort(req);

        ChatRequest {
            model: req.model.0.clone(),
            messages,
            temperature: req.temperature,
            max_tokens: req.max_tokens.or(Some(self.default_max_tokens)),
            top_p: req.top_p,
            stop: req.stop.clone(),
            stream: false,
            reasoning_effort,
            tools,
            extra: req.extra_body.clone(),
        }
    }

    fn build_messages(&self, req: &LlmRequest) -> Vec<ChatMessage> {
        let mut out = Vec::new();

        if let Some(ref sys) = req.system {
            out.push(ChatMessage {
                role: "system".into(),
                content: Some(serde_json::Value::String(sys.clone())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }

        // Phase 4 KV cache：找到最后一条 user message 索引（详见 deepseek.rs 同位置注释）
        let last_user_idx = req.messages.iter().enumerate().rev()
            .find(|(_, m)| matches!(m.role, MessageRole::User))
            .map(|(i, _)| i);

        for (idx, msg) in req.messages.iter().enumerate() {
            let role = match msg.role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
            };

            let content = msg.content.as_ref().map(|c| match c {
                MessageContent::Text(t) => {
                    if Some(idx) == last_user_idx {
                        if let Some(ref preamble) = req.user_message_preamble {
                            return serde_json::Value::String(format!("{}\n\n{}", preamble, t));
                        }
                    }
                    serde_json::Value::String(t.clone())
                },
                MessageContent::MultiPart(parts) => {
                    // Phase 4 fix：vision/multi-modal user message 也需注入 preamble
                    // 兼容 OpenAI 格式：parts = [{"type":"text",...},{"type":"image_url",...}]
                    let mut value = serde_json::to_value(parts).unwrap_or_default();
                    if Some(idx) == last_user_idx {
                        if let Some(ref preamble) = req.user_message_preamble {
                            if let serde_json::Value::Array(ref mut arr) = value {
                                arr.insert(0, serde_json::json!({
                                    "type": "text",
                                    "text": preamble,
                                }));
                            }
                        }
                    }
                    value
                }
            });

            let tool_calls = msg.tool_calls.as_ref().map(|calls| {
                calls
                    .iter()
                    .map(|tc| ToolCall {
                        id: tc.id.clone(),
                        type_: tc.type_.clone(),
                        function: ToolCallFunction {
                            name: tc.function.name.clone(),
                            arguments: tc.function.arguments.clone(),
                        },
                    })
                    .collect()
            });

            out.push(ChatMessage {
                role: role.into(),
                content,
                name: msg.name.clone(),
                tool_calls,
                tool_call_id: msg.tool_call_id.clone(),
            });
        }

        out
    }

    fn parse_response(&self, raw: ChatResponse) -> Result<LlmResponse> {
        let choice = raw
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| KernelError::Provider("empty choices in response".into()))?;

        let content = choice.message.content.unwrap_or_default();
        let finish_reason = choice.finish_reason;

        let msg = Message {
            role: MessageRole::Assistant,
            content: Some(MessageContent::Text(content)),
            name: None,
            tool_calls: choice.message.tool_calls.map(|calls| {
                calls
                    .into_iter()
                    .map(|tc| crate::llm::provider::ToolCall {
                        id: tc.id,
                        type_: tc.type_,
                        function: crate::llm::provider::ToolFunction {
                            name: tc.function.name,
                            arguments: tc.function.arguments,
                        },
                    })
                    .collect()
            }),
            tool_call_id: None,
            reasoning_content: None,
            prefix: false,
        };

        let usage = raw.usage.unwrap_or(Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            prompt_tokens_details: None,
            completion_tokens_details: None,
        });

        let model = if raw.model.is_empty() {
            self.model.clone()
        } else {
            ModelId(raw.model)
        };

        // 提取缓存命中 token（OpenAI/Qwen/Moonshot/SiliconFlow/GLM/xAI 共用此字段）
        let cached = usage.prompt_tokens_details
            .as_ref()
            .map(|d| d.cached_tokens)
            .unwrap_or(0);
        // Qwen 显式缓存返回创建成本，其他平台为 0
        let cache_creation = usage.prompt_tokens_details
            .as_ref()
            .map(|d| d.cache_creation_input_tokens)
            .unwrap_or(0);
        // V30：OpenAI o-series / GPT-5 reasoning_tokens（completion 子集）
        let thinking_tokens = usage.completion_tokens_details
            .as_ref()
            .map(|d| d.reasoning_tokens)
            .unwrap_or(0);

        Ok(LlmResponse {
            model,
            message: msg,
            finish_reason,
            usage: TokenUsage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                // V30 不变量：total = prompt + completion（不再信任 API total）
                total_tokens: usage.prompt_tokens + usage.completion_tokens,
                cached_tokens: cached,
                cache_creation_tokens: cache_creation,
                thinking_tokens,
            },
            thinking: None,
            cache_stats: if cached > 0 || cache_creation > 0 {
                Some(crate::llm::provider::CacheStats {
                    cache_creation_tokens: cache_creation,
                    cache_read_tokens: cached,
                })
            } else {
                None
            },
        })
    }
}

#[async_trait]
impl LlmProvider for OpenAICompatibleProvider {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse> {
        let body = self.build_request(&req);

        debug!(
            model = %body.model,
            messages = %body.messages.len(),
            tools = %body.tools.as_ref().map_or(0, |t| t.len()),
            base_url = %self.base_url,
            "OpenAI-compatible completions request"
        );

        // V18 wire trace（openai-compatible 路径）
        if let Ok(json) = serde_json::to_string_pretty(&body) {
            let _ = std::fs::write("/tmp/abacus_wire_last.json",
                format!("// PROVIDER: openai-compatible\n// BASE_URL: {}\n{}", self.base_url, json));
        }

        let mut retries: u64 = 0;
        let max_retries = 5;

        let resp = loop {
            let auth_value = format!("{}{}", self.auth_prefix, self.api_key.as_str());
            let result = self
                .client
                .post(format!("{}/v1/chat/completions", self.base_url))
                .timeout(self.request_timeout) // H8: per-request timeout
                .header(&self.auth_header, auth_value)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await;

            let resp = match result {
                Ok(r) => r,
                Err(e) => {
                    if retries < max_retries {
                        retries += 1;
                        let delay = Duration::from_secs(retries);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(KernelError::Provider(format!("request failed: {e}")));
                }
            };

            let status = resp.status();

            if (status.as_u16() == 429 || status.is_server_error()) && retries < max_retries {
                retries += 1;
                let delay = Duration::from_secs(retries);
                tokio::time::sleep(delay).await;
                continue;
            }

            break resp;
        };

        let status = resp.status();

        if status.as_u16() == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(30);
            return Err(KernelError::RateLimited { retry_after });
        }

        if status.is_client_error() || status.is_server_error() {
            let body_text = resp.text().await.unwrap_or_default();
            if status.as_u16() == 401 {
                return Err(KernelError::Unauthorized(body_text));
            }
            // V18：openai-compatible 路径错误增强
            let summary = body.messages.iter().enumerate()
                .map(|(i, m)| format!("  [{}] role={}", i, m.role))
                .collect::<Vec<_>>().join("\n");
            let enriched = format!(
                "{}\n--- REQ DUMP (V18 openai-compat) ---\nbase_url={}\nmessages:\n{}\n(完整 JSON 见 /tmp/abacus_wire_last.json)",
                body_text, self.base_url, summary
            );
            return Err(KernelError::ApiError {
                status: status.as_u16(),
                body: enriched,
            });
        }

        let raw: ChatResponse = resp
            .json()
            .await
            .map_err(|e| KernelError::Provider(format!("response parse failed: {e}")))?;

        debug!(
            model = %raw.model,
            usage = ?raw.usage,
            "OpenAI-compatible completions response"
        );

        self.parse_response(raw)
    }

    fn cacheable_segments(&self, req: &LlmRequest) -> Vec<CachedSegment> {
        let mut segments = Vec::new();

        if let Some(ref sys) = req.system {
            segments.push(CachedSegment {
                kind: CachedSegmentKind::SystemPrompt,
                content: sys.clone(),
                breakpoint_after: true,
            });
        }

        if !req.tools.is_empty() {
            let tool_json = serde_json::to_string(&req.tools).unwrap_or_default();
            segments.push(CachedSegment {
                kind: CachedSegmentKind::ToolDefinitions,
                content: tool_json,
                breakpoint_after: true,
            });
        }

        segments
    }

    /// V0.2: OpenAI-format SSE streaming (same format as DeepSeek)
    async fn stream_complete(
        &self,
        req: LlmRequest,
        tx: tokio::sync::mpsc::UnboundedSender<crate::llm::stream::StreamEvent>,
    ) -> Result<LlmResponse> {
        use crate::llm::stream::StreamEvent;
        use futures_util::StreamExt;

        let mut body = self.build_request(&req);
        body.stream = true;

        // V18 wire trace（openai-compatible streaming 路径）
        if let Ok(json) = serde_json::to_string_pretty(&body) {
            let _ = std::fs::write("/tmp/abacus_wire_last.json",
                format!("// PROVIDER: openai-compatible (streaming)\n// BASE_URL: {}\n{}", self.base_url, json));
        }

        let auth_value = format!("{}{}", self.auth_prefix, self.api_key.as_str());
        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .timeout(self.request_timeout) // H8: per-request timeout
            .header(&self.auth_header, auth_value)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| KernelError::Provider(format!("stream request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            // V18：openai-compatible streaming 错误增强
            let summary = body.messages.iter().enumerate()
                .map(|(i, m)| format!("  [{}] role={}", i, m.role))
                .collect::<Vec<_>>().join("\n");
            let enriched = format!(
                "{}\n--- REQ DUMP (V18 openai-compat stream) ---\nbase_url={}\nmessages:\n{}\n(完整 JSON 见 /tmp/abacus_wire_last.json)",
                body_text, self.base_url, summary
            );
            return Err(KernelError::ApiError { status: status.as_u16(), body: enriched });
        }

        let mut byte_stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut prompt_tokens = 0u64;
        let mut completion_tokens = 0u64;
        let mut cached_tokens_stream = 0u64;
        // V30：流式末 chunk 解析的 reasoning_tokens（completion 子集）
        let mut thinking_tokens_stream = 0u64;

        while let Some(chunk) = byte_stream.next().await {
            let bytes = chunk.map_err(|e| KernelError::Provider(format!("stream read: {e}")))?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if line.is_empty() || line.starts_with(':') { continue; }

                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        let _ = tx.send(StreamEvent::Done);
                        break;
                    }
                    if let Ok(chunk_json) = serde_json::from_str::<serde_json::Value>(data) {
                        if let Some(choices) = chunk_json.get("choices").and_then(|c| c.as_array()) {
                            for choice in choices {
                                if let Some(delta) = choice.get("delta") {
                                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                                        if !content.is_empty() {
                                            full_text.push_str(content);
                                            let _ = tx.send(StreamEvent::TextDelta(content.to_string()));
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(usage) = chunk_json.get("usage") {
                            prompt_tokens = usage.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                            completion_tokens = usage.get("completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                            // OpenAI/Qwen/Moonshot 流式最终 chunk 中的 KV 缓存命中统计
                            cached_tokens_stream = usage
                                .get("prompt_tokens_details")
                                .and_then(|d| d.get("cached_tokens"))
                                .and_then(|t| t.as_u64())
                                .unwrap_or(0);
                            // V30：reasoning_tokens（OpenAI o-series / GPT-5 thinking 子集）
                            thinking_tokens_stream = usage
                                .get("completion_tokens_details")
                                .and_then(|d| d.get("reasoning_tokens"))
                                .and_then(|t| t.as_u64())
                                .unwrap_or(0);
                        }
                    }
                }
            }
        }

        let _ = tx.send(StreamEvent::Usage { prompt_tokens, completion_tokens });
        let _ = tx.send(StreamEvent::Done);

        Ok(LlmResponse {
            model: ModelId(body.model),
            message: Message {
                role: MessageRole::Assistant,
                content: Some(MessageContent::Text(full_text)),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                prefix: false,
            },
            finish_reason: "stop".to_string(),
            usage: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                cached_tokens: cached_tokens_stream,
                cache_creation_tokens: 0,
                // V30：流式 thinking_tokens 由末 chunk usage.completion_tokens_details 解析
                thinking_tokens: thinking_tokens_stream,
            },
            thinking: None,
            cache_stats: if cached_tokens_stream > 0 {
                Some(crate::llm::provider::CacheStats {
                    cache_creation_tokens: 0,
                    cache_read_tokens: cached_tokens_stream,
                })
            } else {
                None
            },
        })
    }

    fn provider_id(&self) -> &str {
        "openai-compatible"
    }

    fn supported_models(&self) -> Vec<ModelId> {
        vec![self.model.clone()]
    }

    /// 调用 GET {base_url}/v1/models 拉取可用模型列表（OpenAI 标准协议）
    ///
    /// 响应格式：`{"data": [{"id": "...", "object": "model"}, ...]}`
    /// 失败时返回 Err，调用方应 fallback 到 supported_models().
    async fn discover_models(&self) -> abacus_types::Result<Vec<ModelId>> {
        let auth_value = format!("{}{}", self.auth_prefix, self.api_key.as_str());
        let resp = self.client
            .get(format!("{}/v1/models", self.base_url))
            .timeout(std::time::Duration::from_secs(15)) // discover 短超时（不让首次启动卡死）
            .header(&self.auth_header, auth_value)
            .send()
            .await
            .map_err(|e| abacus_types::KernelError::Provider(format!("discover models: {e}")))?;
        if !resp.status().is_success() {
            return Err(abacus_types::KernelError::ApiError {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        #[derive(serde::Deserialize)]
        struct ModelEntry { id: String }
        #[derive(serde::Deserialize)]
        struct ModelList { data: Vec<ModelEntry> }
        let parsed: ModelList = resp.json().await
            .map_err(|e| abacus_types::KernelError::Provider(format!("parse models list: {e}")))?;
        Ok(parsed.data.into_iter().map(|e| ModelId(e.id)).collect())
    }
}

impl std::fmt::Debug for OpenAICompatibleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAICompatibleProvider")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("auth_header", &self.auth_header)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::provider::{Message, MessageContent, MessageRole, ToolDefinition, ToolFunctionSpec};

    fn make_provider() -> OpenAICompatibleProvider {
        OpenAICompatibleProvider::new(
            "test-key".into(),
            ModelId("gpt-4o".into()),
            "https://api.openai.com".into(),
            None, None, None,
        )
    }

    fn basic_req() -> LlmRequest {
        LlmRequest {
            model: ModelId("gpt-4o".into()),
            messages: vec![Message {
                role: MessageRole::User,
                content: Some(MessageContent::Text("what is 2+2?".into())),
                name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
            }],
            system: Some("Be precise.".into()),
            tools: vec![],
            temperature: Some(0.5),
            max_tokens: Some(50),
            top_p: None, stop: vec![], stream: false,
            thinking_intent: None, extra_body: Default::default(),
            cache_config: None, system_segments: vec![],
            user_message_preamble: None,
        }
    }

    #[test]
    fn test_build_request_model_and_temp() {
        let p = make_provider();
        let req = p.build_request(&basic_req());
        assert_eq!(req.model, "gpt-4o");
        assert_eq!(req.temperature, Some(0.5));
        assert_eq!(req.max_tokens, Some(50));
        assert!(!req.stream);
    }

    #[test]
    fn test_build_request_system_injected_first() {
        let p = make_provider();
        let req = p.build_request(&basic_req());
        assert!(!req.messages.is_empty());
        assert_eq!(req.messages[0].role, "system");
        let sys_content = req.messages[0].content.as_ref()
            .and_then(|v| v.as_str()).unwrap_or("");
        assert!(sys_content.contains("precise"), "system prompt should be injected");
    }

    #[test]
    fn test_build_request_tools_serialized() {
        let p = make_provider();
        let mut r = basic_req();
        r.tools = vec![ToolDefinition {
            type_: "function".into(),
            function: ToolFunctionSpec {
                name: "db_query".into(),
                description: Some("query db".into()),
                parameters: serde_json::json!({"type": "object", "properties": {"sql": {"type": "string"}}, "required": ["sql"]}),
                strict: None,
            },
        }];
        let req = p.build_request(&r);
        let tools = req.tools.expect("tools should be set");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "db_query");
        // Parameters should be the correct JSON schema
        assert!(tools[0].function.parameters["properties"]["sql"].is_object());
    }

    #[test]
    fn test_build_request_no_tools_when_empty() {
        let p = make_provider();
        let req = p.build_request(&basic_req());
        assert!(req.tools.is_none(), "empty tools → None");
    }

    #[test]
    fn test_tool_result_message_has_tool_call_id() {
        let p = make_provider();
        let mut r = basic_req();
        r.messages.push(Message {
            role: MessageRole::Tool,
            content: Some(MessageContent::Text(r#"{"rows": []}"#.into())),
            name: None, tool_calls: None,
            tool_call_id: Some("call_xyz".into()),
            reasoning_content: None,
            prefix: false,
        });
        let req = p.build_request(&r);
        let tool_msg = req.messages.iter().find(|m| m.role == "tool")
            .expect("should have tool message");
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_xyz"));
    }

    #[test]
    fn test_auth_header_custom() {
        let p = OpenAICompatibleProvider::new(
            "Bearer my-token".into(),
            ModelId("custom".into()),
            "https://llm.internal".into(),
            Some("X-API-Key".into()),
            Some("".into()), // no prefix (auth_prefix)
            None, // timeout_secs
        );
        assert_eq!(p.auth_header, "X-API-Key");
    }

    // ── Phase 2 wire-format 测试：reasoning_effort 解析 ─────────────────────

    /// Phase 2：thinking_intent 优先于旧 thinking 字段
    #[test]
    fn test_intent_overrides_legacy_thinking() {
        let p = make_provider();
        let mut r = basic_req();
        r.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::Minimal,
        ));
        // L1 单通道：旧 thinking 字段已删除，下面只能用 thinking_intent。
        // 这条测试本意是验证「显式 thinking_intent 不被 fallback 覆盖」，行为不变。

        let req = p.build_request(&r);
        assert_eq!(req.reasoning_effort.as_deref(), Some("minimal"),
                   "thinking_intent 显式 Minimal");
    }

    /// Phase 2：GPT-5 系列 Minimal 档位透传
    #[test]
    fn test_gpt5_minimal_serializes_correctly() {
        let p = make_provider();
        let mut r = basic_req();
        r.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::Minimal,
        ));
        let req = p.build_request(&r);
        assert_eq!(req.reasoning_effort.as_deref(), Some("minimal"));
    }

    /// Phase 2：Max/XHigh 档位降级到 high（OpenAI 没更高档位）
    #[test]
    fn test_max_and_xhigh_clamp_to_high() {
        let p = make_provider();
        let mut r = basic_req();

        r.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::Max,
        ));
        let req = p.build_request(&r);
        assert_eq!(req.reasoning_effort.as_deref(), Some("high"));

        let mut r2 = basic_req();
        r2.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::XHigh,
        ));
        let req2 = p.build_request(&r2);
        assert_eq!(req2.reasoning_effort.as_deref(), Some("high"));
    }

    /// Phase 2：Adaptive intent 在 OpenAI 路径下转为 None（OpenAI 无 adaptive 字段）
    #[test]
    fn test_adaptive_intent_drops_for_openai() {
        let p = make_provider();
        let mut r = basic_req();
        r.thinking_intent = Some(abacus_types::ThinkingIntent::Adaptive);
        let req = p.build_request(&r);
        assert!(req.reasoning_effort.is_none(),
                "OpenAI 不接受 adaptive，让模型用默认档位");
    }

    /// Phase 2：Off intent 不发 reasoning_effort
    #[test]
    fn test_off_intent_no_effort_field() {
        let p = make_provider();
        let mut r = basic_req();
        r.thinking_intent = Some(abacus_types::ThinkingIntent::Off);
        let req = p.build_request(&r);
        assert!(req.reasoning_effort.is_none());
    }

    /// Phase 2：Budget intent 在 OpenAI 路径下被丢弃（API 不接受 budget）
    #[test]
    fn test_budget_intent_drops_for_openai() {
        let p = make_provider();
        let mut r = basic_req();
        r.thinking_intent = Some(abacus_types::ThinkingIntent::Budget(8192));
        let req = p.build_request(&r);
        assert!(req.reasoning_effort.is_none());
    }

    // ── Insta snapshot 矩阵：OpenAI ───────────────────────────────────────

    fn snapshot_openai_fields(body: &ChatRequest) -> serde_json::Value {
        let json = serde_json::to_value(body).unwrap();
        serde_json::json!({
            "model": json["model"],
            "reasoning_effort": json["reasoning_effort"],
            "stream": json["stream"],
        })
    }

    #[test]
    fn snapshot_openai_gpt5_minimal() {
        let p = make_provider();
        let mut r = basic_req();
        r.model = ModelId("gpt-5".into());
        r.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::Minimal,
        ));
        let body = p.build_request(&r);
        insta::assert_json_snapshot!(snapshot_openai_fields(&body));
    }

    #[test]
    fn snapshot_openai_gpt5_off() {
        let p = make_provider();
        let mut r = basic_req();
        r.model = ModelId("gpt-5".into());
        r.thinking_intent = Some(abacus_types::ThinkingIntent::Off);
        let body = p.build_request(&r);
        insta::assert_json_snapshot!(snapshot_openai_fields(&body));
    }

    #[test]
    fn snapshot_openai_o3_high() {
        let p = make_provider();
        let mut r = basic_req();
        r.model = ModelId("o3".into());
        r.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::High,
        ));
        let body = p.build_request(&r);
        insta::assert_json_snapshot!(snapshot_openai_fields(&body));
    }

    #[test]
    fn snapshot_openai_max_xhigh_clamps_to_high() {
        // OpenAI 没有 max/xhigh 档位 → 应当 clamp 到 high
        let p = make_provider();
        let mut r = basic_req();
        r.model = ModelId("gpt-5".into());
        r.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::XHigh,
        ));
        let body = p.build_request(&r);
        insta::assert_json_snapshot!(snapshot_openai_fields(&body));
    }
}
