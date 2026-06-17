//! Phase 5: Gemini provider — Google AI Generative Language API。
//!
//! ## 范围
//! 本文件提供：
//! 1. Gemini wire-format 类型 + serde（thinkingConfig schema 与 Anthropic/DeepSeek 完全异构）
//! 2. `resolve_thinking()` 把 [`ThinkingIntent`] 翻译成 `ThinkingConfigPayload`
//! 3. Provider struct + LlmProvider trait 实现（HTTP 路径暂占位返回 NotImplemented）
//! 4. wire-format 单元测试
//!
//! ## 与现有 provider 的不同
//! - **认证**：API key 通过 URL `?key=` 或 `x-goog-api-key` header
//! - **endpoint**：`POST /v1beta/models/{model}:generateContent`，模型名嵌在 path
//! - **schema**：消息字段叫 `contents`，role 是 `user|model`（非 `assistant`），
//!   system prompt 是顶层 `systemInstruction`
//! - **thinking**：`generationConfig.thinkingConfig.thinkingBudget` int
//!   - `-1`：动态（模型自决）
//!   - `0`：禁用
//!   - 正整数：上限
//! - **输出**：response.candidates[].content.parts[] 每段可能 `thought:true`（思考）或纯 text
//!
//! ## 引用关系
//! - 创建：`engine_init.rs` / `server.rs` 启动时（待 Phase 5 完整接入）
//! - 消费：pipeline 通过 LlmProvider trait 调用
//!
//! ## 当前状态（截至 G1 实装完成）
//! - ✅ Wire-format schema 完整
//! - ✅ resolve_thinking 全档位映射
//! - ✅ wire-format 单元测试通过
//! - ✅ `complete()` / `stream_complete()` 已接 reqwest + retry + SSE 解析
//! - 🔜 真实 endpoint 烟测仍未跑（需要有效 GOOGLE_API_KEY）

use abacus_types::{KernelError, ModelId, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::debug;

use crate::llm::prompt_cache::CachedSegment;
use crate::llm::provider::{
    CacheStats, LlmProvider, LlmRequest, LlmResponse, Message, MessageContent, MessageRole,
    TokenUsage,
};

// ── Secret string that zeros on drop ────────────────────────────────────
//
// 与 anthropic.rs / deepseek.rs 同样的 zeroize 包装——本地复制保留 provider 私有，
// 避免在跨 provider 共享数据流中无意暴露 api_key 的内存表示。
struct SecretString(zeroize::Zeroizing<String>);

impl SecretString {
    fn new(s: String) -> Self { Self(zeroize::Zeroizing::new(s)) }
    fn as_str(&self) -> &str { &self.0 }
}

// ── Gemini wire-format types ───────────────────────────────────────────────

#[derive(Serialize, Debug)]

pub(crate) struct GeminiRequest {
    pub contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "systemInstruction")]
    pub system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "generationConfig")]
    pub generation_config: Option<GenerationConfig>,
    /// Tool definitions for function calling
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GeminiTool>>,
}

#[derive(Serialize, Debug)]

pub(crate) struct GeminiContent {
    /// "user" / "model" / "system"
    pub role: String,
    pub parts: Vec<GeminiPart>,
}

#[derive(Serialize, Debug)]

pub(crate) struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Tool result (request-side): function response from a previous tool call
    #[serde(skip_serializing_if = "Option::is_none", rename = "functionResponse")]
    pub function_response: Option<GeminiFunctionResponse>,
}

#[derive(Serialize, Debug)]

pub(crate) struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "maxOutputTokens")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "topP")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "thinkingConfig")]
    pub thinking_config: Option<ThinkingConfigPayload>,
}

/// Gemini `thinkingConfig` 字段。注意：`thinkingBudget` 类型是 i32 因为支持 -1（dynamic）。
#[derive(Serialize, Debug, PartialEq, Eq)]

pub(crate) struct ThinkingConfigPayload {
    #[serde(rename = "thinkingBudget")]
    pub thinking_budget: i32,
    #[serde(skip_serializing_if = "Option::is_none", rename = "includeThoughts")]
    pub include_thoughts: Option<bool>,
}

#[derive(Deserialize, Debug)]

pub(crate) struct GeminiResponse {
    pub candidates: Vec<GeminiCandidate>,
    #[serde(default, rename = "usageMetadata")]
    pub usage_metadata: Option<GeminiUsage>,
}

#[derive(Deserialize, Debug)]

pub(crate) struct GeminiCandidate {
    pub content: GeminiResponseContent,
    #[serde(default, rename = "finishReason")]
    pub finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]

pub(crate) struct GeminiResponseContent {
    pub parts: Vec<GeminiResponsePart>,
    /// 反序列化对齐字段——response 可能含 role:"model"，但 parse_response 只读 parts。
    /// 保留以兼容未来需要按 role 区分多 candidate 场景。
    #[serde(default)]
    #[allow(dead_code)]
    pub role: Option<String>,
}

/// Gemini `functionResponse` part (request-side: tool results sent back to model)
#[derive(Serialize, Debug)]
pub(crate) struct GeminiFunctionResponse {
    pub name: String,
    pub response: GeminiFunctionResponseContent,
}

#[derive(Serialize, Debug)]
pub(crate) struct GeminiFunctionResponseContent {
    pub name: String,
    pub content: String,
}

/// Gemini `functionCall` part (response-side: tool calls from model)
#[derive(Deserialize, Debug)]
pub(crate) struct GeminiFunctionCall {
    pub name: String,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
}

/// Gemini tool definition (request-side)
#[derive(Serialize, Debug)]
pub(crate) struct GeminiTool {
    #[serde(rename = "functionDeclarations")]
    pub function_declarations: Vec<GeminiFunctionDeclaration>,
}

/// Gemini function declaration (request-side)
#[derive(Serialize, Debug)]
pub(crate) struct GeminiFunctionDeclaration {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug, Default)]

pub(crate) struct GeminiResponsePart {
    #[serde(default)]
    pub text: Option<String>,
    /// 仅 thinking part 携带此字段（true）
    #[serde(default)]
    pub thought: Option<bool>,
    /// Tool call from model (response-side)
    #[serde(default, rename = "functionCall")]
    pub function_call: Option<GeminiFunctionCall>,
}

#[derive(Deserialize, Debug, Default)]

pub(crate) struct GeminiUsage {
    #[serde(default, rename = "promptTokenCount")]
    pub prompt_token_count: u64,
    #[serde(default, rename = "candidatesTokenCount")]
    pub candidates_token_count: u64,
    #[serde(default, rename = "thoughtsTokenCount")]
    pub thoughts_token_count: u64,
    /// V30 后改为 manual sum；字段保留供未来诊断比对
    #[serde(default, rename = "totalTokenCount")]
    #[allow(dead_code)]
    pub total_token_count: u64,
}

// ── Provider ───────────────────────────────────────────────────────────────

pub struct GeminiProvider {
    /// 进程级共享 client（DNS cache + TLS handshake state + connection pool 共享）
    client: Client,
    /// per-request timeout（H8 模式：从 Client.builder.timeout 移到 RequestBuilder.timeout）
    request_timeout: Duration,
    api_key: SecretString,
    pub(crate) model: ModelId,
    pub(crate) base_url: String,
    default_max_tokens: u32,
}

impl GeminiProvider {
    pub const DEFAULT_BASE_URL: &'static str = "https://generativelanguage.googleapis.com";

    pub fn new(api_key: String, model: ModelId, base_url: Option<String>) -> Self {
        Self::with_config(api_key, model, base_url, None)
    }

    /// Create with explicit timeout（与 anthropic / deepseek with_config 对齐）
    pub fn with_config(
        api_key: String,
        model: ModelId,
        base_url: Option<String>,
        timeout_secs: Option<u64>,
    ) -> Self {
        let client = crate::llm::shared_http_client().clone();
        let request_timeout = Duration::from_secs(timeout_secs.unwrap_or(600));
        Self {
            client,
            request_timeout,
            api_key: SecretString::new(api_key),
            model,
            base_url: base_url.unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string()),
            default_max_tokens: 64000,
        }
    }

    
    /// Phase 5：把 ThinkingIntent 翻译为 Gemini thinkingConfig payload。
    ///
    /// ## 规则
    /// - `Off` → `thinkingBudget: 0`
    /// - `Adaptive` → `thinkingBudget: -1`（dynamic）
    /// - `Effort(level)` → `thinkingBudget: level.default_budget_tokens()`
    /// - `Budget(n)` → `thinkingBudget: n`
    /// - 不在能力区间内的预算由 catalog/resolver 在调用前 clamp
    pub(crate) fn resolve_thinking(intent: &abacus_types::ThinkingIntent) -> ThinkingConfigPayload {
        use abacus_types::ThinkingIntent;
        let thinking_budget = match intent {
            ThinkingIntent::Off => 0i32,
            ThinkingIntent::Adaptive => -1,
            ThinkingIntent::Effort(level) => level.default_budget_tokens() as i32,
            ThinkingIntent::Budget(n) => (*n).min(i32::MAX as u32) as i32,
        };
        ThinkingConfigPayload {
            thinking_budget,
            include_thoughts: Some(true),
        }
    }

    
    pub(crate) fn build_request(&self, req: &LlmRequest) -> GeminiRequest {
        // 把 messages 转成 Gemini contents
        let mut contents = Vec::new();
        // Phase 4 KV cache：找到最后一条 User message 在 req.messages 中的索引
        // （Gemini 跳过 System/Tool role，所以索引相对 contents 不一致；这里用 req.messages 的索引）
        let last_user_idx = req.messages.iter().enumerate().rev()
            .find(|(_, m)| matches!(m.role, crate::llm::provider::MessageRole::User))
            .map(|(i, _)| i);

        // 2026-05-28: 收集中间 System 消息，追加到 systemInstruction
        let mut extra_system_parts: Vec<String> = Vec::new();

        for (idx, msg) in req.messages.iter().enumerate() {
            let role = match msg.role {
                crate::llm::provider::MessageRole::User => "user",
                crate::llm::provider::MessageRole::Assistant => "model",
                crate::llm::provider::MessageRole::Tool => {
                    // Tool results: use native functionResponse part
                    let tool_name = msg.name.as_deref().unwrap_or("tool");
                    let content = match &msg.content {
                        Some(crate::llm::provider::MessageContent::Text(t)) => t.clone(),
                        _ => "{}".to_string(),
                    };
                    contents.push(GeminiContent {
                        role: "user".into(),
                        parts: vec![GeminiPart {
                            text: None,
                            function_response: Some(GeminiFunctionResponse {
                                name: tool_name.to_string(),
                                response: GeminiFunctionResponseContent {
                                    name: tool_name.to_string(),
                                    content,
                                },
                            }),
                        }],
                    });
                    continue;
                }
                crate::llm::provider::MessageRole::System => {
                    // 2026-05-28: 中间 System 消息收集后追加到 systemInstruction
                    // Gemini 不支持 system role 在 contents 中间
                    if let Some(crate::llm::provider::MessageContent::Text(t)) = &msg.content {
                        if !t.is_empty() {
                            extra_system_parts.push(t.clone());
                        }
                    }
                    continue;
                }
            };
            let mut text = match &msg.content {
                Some(crate::llm::provider::MessageContent::Text(t)) => t.clone(),
                Some(crate::llm::provider::MessageContent::MultiPart(parts)) => parts.iter()
                    .filter_map(|p| match p {
                        crate::llm::provider::ContentPart::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
                None => String::new(),
            };
            // Phase 4：仅最后一条 user message 拼 preamble
            if Some(idx) == last_user_idx {
                if let Some(ref preamble) = req.user_message_preamble {
                    text = format!("{}\n\n{}", preamble, text);
                }
            }
            contents.push(GeminiContent {
                role: role.into(),
                parts: vec![GeminiPart { text: Some(text), function_response: None }],
            });
        }

        // 2026-05-28: systemInstruction = 顶层 system prompt + 中间注入的 System 消息
        let system_instruction = {
            let mut sys_text = req.system.clone().unwrap_or_default();
            if !extra_system_parts.is_empty() {
                sys_text.push_str("\n\n--- Runtime Context ---\n");
                sys_text.push_str(&extra_system_parts.join("\n"));
            }
            if sys_text.is_empty() {
                None
            } else {
                Some(GeminiContent {
                    role: "system".into(),
                    parts: vec![GeminiPart { text: Some(sys_text), function_response: None }],
                })
            }
        };

        // L1 后：thinking_intent 单通道
        let thinking_config = req.thinking_intent.clone()
            .filter(|i| i.is_enabled() || matches!(i, abacus_types::ThinkingIntent::Off))
            .map(|i| Self::resolve_thinking(&i));

        let generation_config = Some(GenerationConfig {
            temperature: req.temperature,
            max_output_tokens: req.max_tokens,
            top_p: req.top_p,
            thinking_config,
        });

        // Tool definitions for function calling
        let tools = if req.tools.is_empty() {
            None
        } else {
            Some(vec![GeminiTool {
                function_declarations: req.tools.iter().map(|t| {
                    GeminiFunctionDeclaration {
                        name: t.function.name.clone(),
                        description: t.function.description.clone(),
                        parameters: Some(t.function.parameters.clone()),
                    }
                }).collect(),
            }])
        };

        GeminiRequest {
            contents,
            system_instruction,
            generation_config,
            tools,
        }
    }

    /// Endpoint URL。method 取 `generateContent`（unary）或 `streamGenerateContent`（SSE）。
    /// **不**把 api_key 拼进 URL——避免 access_log 泄漏；改用 `x-goog-api-key` header。
    /// `streamGenerateContent` 需 `?alt=sse` 让 Google 走 SSE 格式（默认 JSON Lines）。
    fn endpoint_url(&self, method: &str) -> String {
        let mut url = format!("{}/v1beta/models/{}:{}", self.base_url, self.model.0, method);
        if method == "streamGenerateContent" {
            url.push_str("?alt=sse");
        }
        url
    }

    /// 把 GeminiResponse 解析为通用 LlmResponse。
    /// **关键差异**：Gemini 把 thinking 与 text 混在 `parts[]` 数组里，
    /// `thought:true` 标记的 part 是思考内容，其余为最终回答。
    /// `functionCall` 标记的 part 是工具调用请求。
    fn parse_response(&self, raw: GeminiResponse) -> Result<LlmResponse> {
        let candidate = raw.candidates.into_iter().next()
            .ok_or_else(|| KernelError::Provider("empty candidates in Gemini response".into()))?;

        // 把 parts 按类型拆分
        let mut text = String::new();
        let mut thinking = String::new();
        let mut tool_calls: Vec<crate::llm::provider::ToolCall> = Vec::new();
        let mut tool_call_index: u32 = 0;

        for part in &candidate.content.parts {
            // Tool call
            if let Some(fc) = &part.function_call {
                let args_json = match &fc.args {
                    Some(v) => serde_json::to_string(v).unwrap_or_default(),
                    None => "{}".to_string(),
                };
                tool_calls.push(crate::llm::provider::ToolCall {
                    id: format!("call_{}", tool_call_index),
                    type_: "function".to_string(),
                    function: crate::llm::provider::ToolFunction {
                        name: fc.name.clone(),
                        arguments: args_json,
                    },
                });
                tool_call_index += 1;
                continue;
            }

            // Text / Thinking
            if let Some(t) = &part.text {
                if part.thought.unwrap_or(false) {
                    thinking.push_str(t);
                } else {
                    text.push_str(t);
                }
            }
        }

        let usage = raw.usage_metadata.unwrap_or_default();
        let thinking_opt = if thinking.is_empty() { None } else { Some(thinking) };

        let msg = Message {
            role: MessageRole::Assistant,
            content: Some(MessageContent::Text(text)),
            name: None,
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            tool_call_id: None,
            reasoning_content: thinking_opt.clone(),
            prefix: false,
        };

        Ok(LlmResponse {
            model: self.model.clone(),
            message: msg,
            finish_reason: candidate.finish_reason.unwrap_or_else(|| "STOP".into()),
            usage: TokenUsage {
                prompt_tokens: usage.prompt_token_count,
                completion_tokens: usage.candidates_token_count + usage.thoughts_token_count,
                total_tokens: usage.prompt_token_count + usage.candidates_token_count + usage.thoughts_token_count,
                cached_tokens: 0,
                cache_creation_tokens: 0,
                thinking_tokens: usage.thoughts_token_count,
            },
            thinking: thinking_opt,
            cache_stats: Some(CacheStats { cache_creation_tokens: 0, cache_read_tokens: 0 }),
        })
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse> {
        // build_request 已在 Phase 5 完成 wire 类型；这里仅负责 HTTP + 错误映射 + 解析
        let mut body = self.build_request(&req);
        // 兜底 maxOutputTokens：Gemini 服务端默认上限较保守，避免 truncation
        if let Some(gc) = body.generation_config.as_mut() {
            if gc.max_output_tokens.is_none() {
                gc.max_output_tokens = Some(self.default_max_tokens);
            }
        }

        debug!(
            model = %self.model.0,
            messages = %body.contents.len(),
            "Gemini generateContent request"
        );

        let mut retries: u64 = 0;
        let max_retries = 5;
        let url = self.endpoint_url("generateContent");

        let resp = loop {
            let result = self
                .client
                .post(&url)
                .timeout(self.request_timeout)
                .header("x-goog-api-key", self.api_key.as_str())
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await;

            let resp = match result {
                Ok(r) => r,
                Err(e) => {
                    if retries < max_retries {
                        retries += 1;
                        tracing::info!(
                            attempt = retries,
                            max = max_retries,
                            error = %e,
                            "gemini: retrying after transport error"
                        );
                        tokio::time::sleep(Duration::from_secs(retries)).await;
                        continue;
                    }
                    return Err(KernelError::Provider(format!("request failed: {e}")));
                }
            };

            let status = resp.status();
            if (status.as_u16() == 429 || status.is_server_error()) && retries < max_retries {
                retries += 1;
                tokio::time::sleep(Duration::from_secs(retries)).await;
                continue;
            }
            break resp;
        };

        let status = resp.status();
        if status.as_u16() == 429 {
            let retry_after = resp.headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(30);
            return Err(KernelError::RateLimited { retry_after });
        }
        if status.is_client_error() || status.is_server_error() {
            let body_text = resp.text().await.unwrap_or_default();
            // 401 / 403：Google 返回 PERMISSION_DENIED 多走 403；都视作 Unauthorized 让上层 wrap retry
            if matches!(status.as_u16(), 401 | 403) {
                return Err(KernelError::Unauthorized(body_text));
            }
            return Err(KernelError::ApiError {
                status: status.as_u16(),
                body: body_text,
            });
        }

        let raw_text = resp.text().await
            .map_err(|e| KernelError::Provider(format!("read body failed: {e}")))?;

        // Google 错误响应（即使 200）形如 { "error": { "code":..., "message":... } }——少见但需防御
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw_text) {
            if let Some(err) = v.get("error") {
                let code = err.get("code").and_then(|c| c.as_u64()).unwrap_or(0) as u16;
                let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown gemini error");
                return Err(KernelError::ApiError { status: code, body: msg.into() });
            }
        }

        let parsed: GeminiResponse = serde_json::from_str(&raw_text)
            .map_err(|e| KernelError::Provider(format!("response parse failed: {e}")))?;

        debug!(
            model = %self.model.0,
            usage = ?parsed.usage_metadata,
            "Gemini generateContent response"
        );

        self.parse_response(parsed)
    }

    async fn stream_complete(
        &self,
        req: LlmRequest,
        tx: tokio::sync::mpsc::UnboundedSender<crate::llm::stream::StreamEvent>,
    ) -> Result<LlmResponse> {
        use crate::llm::stream::StreamEvent;
        use futures_util::StreamExt;

        // Gemini SSE：?alt=sse → Server-Sent Events 格式；每 event 是一个完整 GeminiResponse 增量。
        // 与 OpenAI 不同的是 chunk 直接是完整 candidates[].content.parts[]，需要按 part 拼接。
        let mut body = self.build_request(&req);
        if let Some(gc) = body.generation_config.as_mut() {
            if gc.max_output_tokens.is_none() {
                gc.max_output_tokens = Some(self.default_max_tokens);
            }
        }
        let url = self.endpoint_url("streamGenerateContent");

        let resp = self
            .client
            .post(&url)
            .timeout(self.request_timeout)
            .header("x-goog-api-key", self.api_key.as_str())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| KernelError::Provider(format!("stream request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            if tx.send(StreamEvent::Error(
                format!("HTTP {}: {}", status.as_u16(), &body_text[..body_text.len().min(200)])
            )).is_err() {
                tracing::debug!("stream consumer gone before HTTP error");
            }
            if matches!(status.as_u16(), 401 | 403) {
                return Err(KernelError::Unauthorized(body_text));
            }
            return Err(KernelError::ApiError { status: status.as_u16(), body: body_text });
        }

        let mut byte_stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut full_thinking = String::new();
        let mut prompt_tokens = 0u64;
        let mut completion_tokens = 0u64;
        let mut thoughts_tokens = 0u64;

        // 统一流式 tool_calls 组装器
        let mut tc_collector = crate::llm::stream::StreamingToolCallCollector::new();
        let mut tool_call_counter: u32 = 0;

        // P2: stream idle timeout — 45s 无新 chunk 视为连接死锁，主动断开
        loop {
            let chunk = match tokio::time::timeout(std::time::Duration::from_secs(45), byte_stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break, // stream 正常结束
                Err(_) => {
                    tracing::warn!("stream idle timeout (45s), treating as complete");
                    if tx.send(StreamEvent::Error("stream idle timeout (45s)".into())).is_err() {
                        tracing::debug!("stream consumer gone, stopping");
                    }
                    break;
                }
            };
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    if tx.send(StreamEvent::Error(
                        format!("stream interrupted: {e}")
                    )).is_err() {
                        tracing::debug!("stream consumer gone during error: {e}");
                    }
                    return Err(KernelError::Provider(format!("stream read: {e}")));
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if line.is_empty() || line.starts_with(':') { continue; }
                let Some(data) = line.strip_prefix("data: ") else { continue; };

                let Ok(chunk_json) = serde_json::from_str::<serde_json::Value>(data) else { continue };

                // candidates[0].content.parts[]：thought:true 部分走 thinking delta
                // functionCall 部分走 tool call 组装
                if let Some(parts) = chunk_json
                    .pointer("/candidates/0/content/parts")
                    .and_then(|p| p.as_array())
                {
                    for part in parts {
                        // Tool call (functionCall)
                        if let Some(fc) = part.get("functionCall") {
                            let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("");
                            let args = fc.get("args");
                            let tc_idx = tool_call_counter;
                            tool_call_counter += 1;

                            let id = format!("call_{}", tc_idx);
                            tc_collector.on_tool_call_start(tc_idx, Some(&id), Some(name), &tx);

                            if let Some(args_val) = args {
                                let args_str = serde_json::to_string(args_val).unwrap_or_default();
                                tc_collector.on_tool_call_args(tc_idx, &args_str, &tx);
                            }
                            tc_collector.on_tool_call_end(tc_idx, &tx);
                            continue;
                        }

                        // Text / Thinking
                        let is_thought = part.get("thought").and_then(|t| t.as_bool()).unwrap_or(false);
                        if let Some(t) = part.get("text").and_then(|s| s.as_str()) {
                            if is_thought {
                                full_thinking.push_str(t);
                                let _ = tx.send(StreamEvent::ThinkingDelta(t.into()));
                            } else if !t.is_empty() {
                                full_text.push_str(t);
                                let _ = tx.send(StreamEvent::TextDelta(t.into()));
                            }
                        }
                    }
                }

                if let Some(usage) = chunk_json.get("usageMetadata") {
                    prompt_tokens = usage.get("promptTokenCount").and_then(|c| c.as_u64()).unwrap_or(prompt_tokens);
                    completion_tokens = usage.get("candidatesTokenCount").and_then(|c| c.as_u64()).unwrap_or(completion_tokens);
                    thoughts_tokens = usage.get("thoughtsTokenCount").and_then(|c| c.as_u64()).unwrap_or(thoughts_tokens);
                }
            }
        }

        if tx.send(StreamEvent::Usage {
            prompt_tokens,
            completion_tokens: completion_tokens + thoughts_tokens,
        }).is_err() {
            tracing::debug!("stream consumer gone before Usage");
        }
        if tx.send(StreamEvent::Done).is_err() {
            tracing::debug!("stream consumer gone before final Done");
        }

        let thinking_opt = if full_thinking.is_empty() { None } else { Some(full_thinking) };

        Ok(LlmResponse {
            model: self.model.clone(),
            message: Message {
                role: MessageRole::Assistant,
                content: Some(MessageContent::Text(full_text)),
                name: None,
                tool_calls: tc_collector.finish(),
                tool_call_id: None,
                reasoning_content: thinking_opt.clone(),
                prefix: false,
            },
            finish_reason: "STOP".to_string(),
            usage: TokenUsage {
                prompt_tokens,
                // V30：thoughts 合并到 completion 作为子集（与非流式路径对齐）
                completion_tokens: completion_tokens + thoughts_tokens,
                // V30：local completion_tokens 是原始 candidates，不含 thoughts，所以
                // total = prompt + candidates + thoughts == prompt + TokenUsage.completion，
                // 满足跨 provider 不变量 total = prompt + completion。
                total_tokens: prompt_tokens + completion_tokens + thoughts_tokens,
                cached_tokens: 0,
                cache_creation_tokens: 0,
                thinking_tokens: thoughts_tokens,
            },
            thinking: thinking_opt,
            cache_stats: Some(CacheStats { cache_creation_tokens: 0, cache_read_tokens: 0 }),
        })
    }

    fn provider_id(&self) -> &str { "gemini" }

    fn supported_models(&self) -> Vec<ModelId> {
        vec![
            ModelId("gemini-2.5-pro".into()),
            ModelId("gemini-2.5-flash".into()),
            ModelId("gemini-2.5-flash-lite".into()),
        ]
    }

    fn cacheable_segments(&self, _req: &LlmRequest) -> Vec<CachedSegment> {
        // Gemini implicit caching 在 v1beta 上不稳定，暂不主动声明可缓存段
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::{EffortLevel, ThinkingIntent};

    fn make_provider() -> GeminiProvider {
        GeminiProvider::new("test-key".into(), ModelId("gemini-2.5-pro".into()), None)
    }

    fn empty_req() -> LlmRequest {
        LlmRequest {
            model: ModelId("gemini-2.5-pro".into()),
            messages: vec![crate::llm::provider::Message {
                role: crate::llm::provider::MessageRole::User,
                content: Some(crate::llm::provider::MessageContent::Text("hello".into())),
                name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
            }],
            system: Some("You are helpful.".into()),
            system_segments: Vec::new(),
            tools: Vec::new(),
            temperature: Some(0.7),
            max_tokens: Some(8192),
            top_p: None, stop: vec![], stream: false,
            thinking_intent: None,
            cache_config: None,
            extra_body: Default::default(),
            user_message_preamble: None,
        }
    }

    #[test]
    fn test_off_intent_zero_budget() {
        let p = make_provider();
        let mut req = empty_req();
        req.thinking_intent = Some(ThinkingIntent::Off);
        let body = p.build_request(&req);
        let tc = body.generation_config.unwrap().thinking_config.unwrap();
        assert_eq!(tc.thinking_budget, 0, "Off → thinkingBudget 0");
    }

    #[test]
    fn test_adaptive_dynamic_negative_one() {
        let p = make_provider();
        let mut req = empty_req();
        req.thinking_intent = Some(ThinkingIntent::Adaptive);
        let body = p.build_request(&req);
        let tc = body.generation_config.unwrap().thinking_config.unwrap();
        assert_eq!(tc.thinking_budget, -1, "Adaptive → -1（dynamic）");
    }

    #[test]
    fn test_effort_high_default_budget() {
        let p = make_provider();
        let mut req = empty_req();
        req.thinking_intent = Some(ThinkingIntent::Effort(EffortLevel::High));
        let body = p.build_request(&req);
        let tc = body.generation_config.unwrap().thinking_config.unwrap();
        assert_eq!(tc.thinking_budget, EffortLevel::High.default_budget_tokens() as i32);
    }

    #[test]
    fn test_explicit_budget_passes_through() {
        let p = make_provider();
        let mut req = empty_req();
        req.thinking_intent = Some(ThinkingIntent::Budget(4096));
        let body = p.build_request(&req);
        let tc = body.generation_config.unwrap().thinking_config.unwrap();
        assert_eq!(tc.thinking_budget, 4096);
    }

    #[test]
    fn test_no_intent_omits_thinking_config() {
        let p = make_provider();
        let req = empty_req();  // thinking_intent + thinking 都为 None
        let body = p.build_request(&req);
        let gc = body.generation_config.unwrap();
        assert!(gc.thinking_config.is_none(), "意图缺省时不发送 thinkingConfig");
    }

    /// L1 单通道：直接通过 thinking_intent 设置 effort。
    /// 原 test_legacy_thinking_lift_to_intent 测的 lift 路径已删除，
    /// 改为验证 thinking_intent 直通 GenerationConfig。
    #[test]
    fn test_thinking_intent_drives_thinking_budget() {
        use abacus_types::{ThinkingIntent, EffortLevel as Lvl};
        let p = make_provider();
        let mut req = empty_req();
        req.thinking_intent = Some(ThinkingIntent::Effort(Lvl::High));
        let body = p.build_request(&req);
        let tc = body.generation_config.unwrap().thinking_config.unwrap();
        assert_eq!(tc.thinking_budget, EffortLevel::High.default_budget_tokens() as i32);
    }

    #[test]
    fn test_system_prompt_goes_to_system_instruction() {
        let p = make_provider();
        let req = empty_req();
        let body = p.build_request(&req);
        // system 字段不在 contents 里
        assert!(body.system_instruction.is_some());
        assert!(body.contents.iter().all(|c| c.role != "system"));
    }

    #[test]
    fn test_assistant_role_renamed_to_model() {
        let p = make_provider();
        let mut req = empty_req();
        req.messages.push(crate::llm::provider::Message {
            role: crate::llm::provider::MessageRole::Assistant,
            content: Some(crate::llm::provider::MessageContent::Text("ok".into())),
            name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
        });
        let body = p.build_request(&req);
        // user + model 各一条
        assert!(body.contents.iter().any(|c| c.role == "model"),
                "Gemini 用 'model' 而非 'assistant'");
    }

    /// G1 之后 complete() 走真实 HTTP——这里仅断言 parse_response 对合成响应的转换。
    /// 真实 endpoint 烟测依赖有效 GOOGLE_API_KEY，放到 e2e 层不在 unit test 中跑。
    #[test]
    fn test_parse_response_splits_thought_and_text() {
        let p = make_provider();
        let resp = GeminiResponse {
            candidates: vec![GeminiCandidate {
                content: GeminiResponseContent {
                    parts: vec![
                        GeminiResponsePart { text: Some("inner thought".into()), thought: Some(true), ..Default::default() },
                        GeminiResponsePart { text: Some("final answer".into()), thought: None, ..Default::default() },
                    ],
                    role: Some("model".into()),
                },
                finish_reason: Some("STOP".into()),
            }],
            usage_metadata: Some(GeminiUsage {
                prompt_token_count: 10,
                candidates_token_count: 5,
                thoughts_token_count: 3,
                total_token_count: 18,
            }),
        };
        let parsed = p.parse_response(resp).expect("parse should succeed");
        let content = match parsed.message.content.as_ref().unwrap() {
            crate::llm::provider::MessageContent::Text(t) => t.clone(),
            _ => panic!("expected text content"),
        };
        assert_eq!(content, "final answer", "thought:true 不进 message.content");
        assert_eq!(parsed.thinking.as_deref(), Some("inner thought"), "thought:true 进 thinking");
        assert_eq!(parsed.usage.prompt_tokens, 10);
        // completion_tokens = candidates + thoughts
        assert_eq!(parsed.usage.completion_tokens, 8);
    }

    /// parse_response：finish_reason 缺省时给出默认值（G1 容错）
    #[test]
    fn test_parse_response_missing_finish_reason_defaults_to_stop() {
        let p = make_provider();
        let resp = GeminiResponse {
            candidates: vec![GeminiCandidate {
                content: GeminiResponseContent {
                    parts: vec![GeminiResponsePart { text: Some("hi".into()), thought: None, ..Default::default() }],
                    role: Some("model".into()),
                },
                finish_reason: None,
            }],
            usage_metadata: None,
        };
        let parsed = p.parse_response(resp).unwrap();
        assert_eq!(parsed.finish_reason, "STOP");
    }

    /// parse_response：空 candidates → Provider error
    #[test]
    fn test_parse_response_empty_candidates_errors() {
        let p = make_provider();
        let resp = GeminiResponse { candidates: vec![], usage_metadata: None };
        assert!(p.parse_response(resp).is_err(), "空 candidates 必须报错");
    }

    // ── Insta snapshot 矩阵：Gemini ───────────────────────────────────────

    fn snapshot_gemini_fields(body: &GeminiRequest) -> serde_json::Value {
        let json = serde_json::to_value(body).unwrap();
        serde_json::json!({
            "generationConfig": json["generationConfig"],
        })
    }

    #[test]
    fn snapshot_gemini_pro_budget_8192() {
        let p = make_provider();
        let mut req = empty_req();
        req.thinking_intent = Some(ThinkingIntent::Budget(8192));
        let body = p.build_request(&req);
        insta::assert_json_snapshot!(snapshot_gemini_fields(&body));
    }

    #[test]
    fn snapshot_gemini_pro_adaptive_dynamic() {
        let p = make_provider();
        let mut req = empty_req();
        req.thinking_intent = Some(ThinkingIntent::Adaptive);
        let body = p.build_request(&req);
        insta::assert_json_snapshot!(snapshot_gemini_fields(&body));
    }

    #[test]
    fn snapshot_gemini_pro_off_zero_budget() {
        let p = make_provider();
        let mut req = empty_req();
        req.thinking_intent = Some(ThinkingIntent::Off);
        let body = p.build_request(&req);
        insta::assert_json_snapshot!(snapshot_gemini_fields(&body));
    }

    #[test]
    fn snapshot_gemini_pro_no_intent() {
        // 缺省意图 → thinkingConfig 字段缺失（非空 GenerationConfig）
        let p = make_provider();
        let req = empty_req();
        let body = p.build_request(&req);
        insta::assert_json_snapshot!(snapshot_gemini_fields(&body));
    }
}
