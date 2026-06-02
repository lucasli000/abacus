//! Anthropic provider — Anthropic Messages API 实现
//!
//! ## 适用
//! - Claude 系列模型（claude-opus-4, claude-sonnet-4, claude-3.5-haiku 等）
//! - 支持 tool use、extended thinking
//!
//! ## 与 OpenAI 的区别
//! - 端点 `/v1/messages`（非 `/v1/chat/completions`）
//! - 认证头 `x-api-key`（非 `Authorization: Bearer`）
//! - system prompt 是顶层字段，不在 messages 数组中
//! - tool_use 以 content block 形式返回（非顶层 tool_calls 字段）
//! - tool_result 以 content block 形式传入（非单独 tool role）
//!
//! ## 配置
//! 环境变量:
//! - `ABACUS_ANTHROPIC_API_KEY` 或 `ANTHROPIC_API_KEY`
//! - `ABACUS_ANTHROPIC_BASE_URL`（默认 `https://api.anthropic.com`）

use abacus_types::{KernelError, ModelId, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, warn};

use crate::llm::prompt_cache::{CachedSegment, CachedSegmentKind};
use crate::llm::provider::{
    CacheStats, LlmProvider, LlmRequest, LlmResponse, Message, MessageContent, MessageRole,
    TokenUsage,
};

struct SecretString(zeroize::Zeroizing<String>);

impl SecretString {
    fn new(s: String) -> Self { Self(zeroize::Zeroizing::new(s)) }
    fn as_str(&self) -> &str { &self.0 }
}

// ── Anthropic API 类型 ────────────────────────────────────────────────

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemBlock>>,
    messages: Vec<AnthropicMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    /// 经典 extended_thinking（Sonnet 4.5/Opus 4.5/Haiku 4 路径）。
    /// Opus 4.7 不再支持此字段；Opus 4.6/Sonnet 4.6 在 deprecation 期内仍然兼容。
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    /// Phase 2：adaptive thinking + effort（Opus 4.7 唯一模式 / Opus 4.6 / Sonnet 4.6 推荐）。
    /// 与 `thinking` 互斥：API 校验，同时存在会 400。
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

/// Phase 2：Anthropic 4.6+ 引入的 `output_config` 顶层字段。
/// 当前仅承载 `effort`（与 thinking.type=adaptive 配套）。
#[derive(Serialize)]
struct OutputConfig {
    /// "low" / "medium" / "high" / "max" / "xhigh"
    effort: String,
}

#[derive(Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    type_: String,
    text: String,
    /// Anthropic prompt caching: marks this block as cacheable
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize, Clone)]
struct CacheControl {
    #[serde(rename = "type")]
    type_: String,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: AnthropicContent,
}

#[derive(Serialize)]
#[serde(untagged)]
enum AnthropicContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        source: ImageSource,
    },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String, input: serde_json::Value },
    #[serde(rename = "tool_result")]
    ToolResult { tool_use_id: String, content: serde_json::Value },
}

#[derive(Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    type_: String,
    media_type: String,
    data: String,
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: serde_json::Value,
}

/// Phase 2：Anthropic thinking 字段的 wire-format 表达。
/// 现支持两种 type：
/// - `enabled` + `budget_tokens`（旧 extended_thinking 路径）
/// - `adaptive`（无 budget_tokens；effort 通过顶层 `output_config.effort` 表达）
///
/// 注意：`budget_tokens` 在 adaptive 模式下不能出现，否则 API 400。
#[derive(Serialize)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    type_: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display: Option<String>,  // "summarized" | "omitted" (adaptive only)
}

/// Phase 2 内部 sealed enum：把 ThinkingIntent 解析后的 provider-native 表达。
/// pipeline 永远只看到 ThinkingIntent；`build_request` 内部把它翻译成此 enum，
/// 再 serialize 到 wire-format 字段。
#[derive(Debug, Clone)]
enum AnthropicThinkingMode {
    /// 不发送 thinking / output_config 字段
    Off,
    /// 旧路径（Sonnet 4.5/Opus 4.5/Haiku 4 等）：thinking.type="enabled"+budget_tokens
    ExtendedThinking { budget_tokens: u32 },
    /// 新路径（Opus 4.7 唯一/Opus 4.6/Sonnet 4.6 推荐）：thinking.type="adaptive" + output_config.effort
    Adaptive { effort: String, display: Option<String> },
}

#[derive(Deserialize)]
struct AnthropicResponse {
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    content: Vec<ResponseBlock>,
    #[serde(default)]
    stop_reason: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ResponseBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[allow(dead_code)]
        #[serde(default)]
        citations: Option<serde_json::Value>,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[allow(dead_code)]
        #[serde(default)]
        signature: Option<String>,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking {
        #[allow(dead_code)]
        data: String,
    },
}

#[derive(Debug, Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

// ── Phase 2 helpers：模型路由 + intent 转换 ─────────────────────────────────

/// Opus 4.7 / Mythos Preview 强制走 adaptive 路径。
/// 旧 thinking={type:"enabled",budget_tokens} 会返 400。
fn is_opus_4_7_or_mythos(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("opus-4-7") || m.contains("opus-4.7") || m.contains("mythos")
}

/// Opus 4.6 / Sonnet 4.6 同时支持 adaptive + extended_thinking，adaptive 推荐。
fn is_anthropic_4_6_dual(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("opus-4-6") || m.contains("opus-4.6")
        || m.contains("sonnet-4-6") || m.contains("sonnet-4.6")
}

/// EffortLevel → Anthropic adaptive 接受的 effort 字符串
fn anthropic_effort_str(level: abacus_types::EffortLevel) -> &'static str {
    use abacus_types::EffortLevel::*;
    match level {
        // Anthropic 没有 minimal 档位 → 降级为 low
        Minimal | Low => "low",
        Medium => "medium",
        High => "high",
        Max => "max",
        XHigh => "xhigh",
    }
}

// L1：legacy_thinking_to_intent 已删除——req.thinking_intent 是单通道。

// ── Provider ───────────────────────────────────────────────────────────

pub struct AnthropicProvider {
    client: Client,
    /// 每请求超时（H8 修复：从 Client.builder.timeout 移到 RequestBuilder.timeout，
    /// 让所有 provider 共享底层连接池 + DNS cache + TLS handshake state）
    request_timeout: Duration,
    api_key: SecretString,
    base_url: String,
    model: ModelId,
    default_max_tokens: u32,
    thinking_budget: u32,
    beta_headers: Vec<(String, String)>,
    /// 是否允许 discover_models() 发网络请求
    /// true = 用户显式配置了 base_url → 允许打 /v1/models
    /// false = 使用内置默认 URL → 只返回静态列表（不发网络请求）
    /// 引用关系：构造函数根据 base_url 参数是否为 Some 设置；discover_models() 消费
    discover_enabled: bool,
}

impl AnthropicProvider {
    const DEFAULT_BASE_URL: &'static str = "https://api.anthropic.com";
    const API_VERSION: &'static str = "2023-06-01";

    pub fn new(
        api_key: String,
        model: impl Into<ModelId>,
        base_url: Option<String>,
        timeout_secs: Option<u64>,
    ) -> Self {
        Self::with_beta_headers(api_key, model, base_url, timeout_secs, Vec::new())
    }

    pub fn with_beta_headers(
        api_key: String,
        model: impl Into<ModelId>,
        base_url: Option<String>,
        timeout_secs: Option<u64>,
        beta_headers: Vec<(String, String)>,
    ) -> Self {
        // H8 修复：复用进程级共享 Client（连接池/DNS/TLS state 跨 provider 共享）
        let client = crate::llm::shared_http_client().clone();
        let request_timeout = Duration::from_secs(timeout_secs.unwrap_or(600));

        let model_id: ModelId = model.into();
        let (budget, max_tokens) = if model_id.0.contains("sonnet") || model_id.0.contains("haiku") {
            (16384, 32000)
        } else if model_id.0.contains("opus") {
            (32768, 32000)
        } else {
            (16384, 32000)
        };

        let discover_enabled = base_url.is_some();
        Self {
            client,
            request_timeout,
            api_key: SecretString::new(api_key),
            base_url: base_url.unwrap_or_else(|| Self::DEFAULT_BASE_URL.into()),
            model: model_id,
            default_max_tokens: max_tokens,
            thinking_budget: budget,
            beta_headers,
            discover_enabled,
        }
    }

    /// 构建 messages endpoint URL
    /// base_url 直接追加 /messages（用户需配置含版本路径的 base_url）
    /// 示例：`https://api.anthropic.com/v1` → `.../v1/messages`
    fn messages_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/messages") {
            base.to_string()
        } else {
            format!("{}/messages", base)
        }
    }

    fn build_request(&self, req: &LlmRequest) -> AnthropicRequest {
        // KV 缓存优化: 多段 system prompt，稳定段标记 cache_control
        // Anthropic 按 block 级别缓存 — 稳定 block 内容不变时缓存命中
        let cache_enabled = req.cache_config.as_ref().map(|c| c.enabled).unwrap_or(false);
        let system = if !req.system_segments.is_empty() {
            // V0.2: 多段模式 — 每段独立 block，cacheable 段标记 cache_control
            Some(req.system_segments.iter().map(|seg| SystemBlock {
                type_: "text".into(),
                text: seg.text.clone(),
                cache_control: if cache_enabled && seg.cacheable {
                    Some(CacheControl { type_: "ephemeral".into() })
                } else {
                    None
                },
            }).collect())
        } else {
            // Fallback: 单一 system string
            req.system.as_ref().map(|s| {
                vec![SystemBlock {
                    type_: "text".into(),
                    text: s.clone(),
                    cache_control: if cache_enabled {
                        Some(CacheControl { type_: "ephemeral".into() })
                    } else {
                        None
                    },
                }]
            })
        };

        let messages = self.build_messages(req);

        let tools = if req.tools.is_empty() {
            None
        } else {
            Some(req.tools.iter().map(|t| AnthropicTool {
                name: t.function.name.clone(),
                description: t.function.description.clone(),
                input_schema: t.function.parameters.clone(),
            }).collect())
        };

        // Phase 2：先解析为 native enum，再分发到 wire-format 字段
        let mode = self.resolve_thinking(req);
        let (thinking, output_config, max_tokens) = match &mode {
            AnthropicThinkingMode::Off => (None, None, None),
            AnthropicThinkingMode::ExtendedThinking { budget_tokens } => {
                // Anthropic 强制：max_tokens > budget_tokens（差至少 1024 留输出空间）
                let mt = req.max_tokens.unwrap_or(self.default_max_tokens).max(*budget_tokens + 1024);
                (
                    Some(AnthropicThinking {
                        type_: "enabled".into(),
                        budget_tokens: Some(*budget_tokens),
                        display: None,
                    }),
                    None,
                    Some(mt),
                )
            }
            AnthropicThinkingMode::Adaptive { effort, display } => {
                (
                    Some(AnthropicThinking {
                        type_: "adaptive".into(),
                        budget_tokens: None,  // adaptive 不能带 budget_tokens
                        display: display.clone(),
                    }),
                    Some(OutputConfig { effort: effort.clone() }),
                    None,  // adaptive 由模型自决，max_tokens 不需特殊放大
                )
            }
        };

        // Anthropic API 要求 max_tokens 字段始终存在
        let final_max_tokens = max_tokens.or(req.max_tokens).unwrap_or(self.default_max_tokens);

        AnthropicRequest {
            model: req.model.0.clone(),
            system,
            messages,
            max_tokens: final_max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            stop_sequences: req.stop.clone(),
            stream: false,
            tools,
            thinking,
            output_config,
            extra: req.extra_body.clone(),
        }
    }

    /// Phase 2：把 `LlmRequest.thinking_intent`（优先）或 `LlmRequest.thinking`
    /// （兼容路径）解析为 Anthropic 内部 native mode。
    ///
    /// ## 决策路径
    /// 1. 用户没传 thinking 信号 → Off
    /// 2. 模型名匹配 Opus 4.7 / Mythos → 强制 Adaptive 路径（旧字段会被 API 拒收）
    /// 3. 模型支持 Adaptive（Opus 4.6 / Sonnet 4.6）且 intent 是 Adaptive 或高档位 → Adaptive
    /// 4. 否则 → ExtendedThinking with budget_tokens
    fn resolve_thinking(&self, req: &LlmRequest) -> AnthropicThinkingMode {
        // Step 1：L1 后单通道——直接读 thinking_intent
        let intent = match req.thinking_intent.clone() {
            Some(i) if i.is_enabled() => i,
            _ => return AnthropicThinkingMode::Off,
        };

        // Step 2：根据 model id 决定走哪条 wire 路径
        let model = req.model.0.as_str();
        let opus_4_7_or_mythos = is_opus_4_7_or_mythos(model);
        let supports_adaptive = opus_4_7_or_mythos || is_anthropic_4_6_dual(model);

        // Step 3：解析为 native mode
        match intent {
            abacus_types::ThinkingIntent::Off => AnthropicThinkingMode::Off,

            abacus_types::ThinkingIntent::Adaptive => {
                if supports_adaptive {
                    AnthropicThinkingMode::Adaptive {
                        effort: "medium".into(),  // adaptive 默认 medium
                        display: None,
                    }
                } else {
                    // 旧模型不支持 adaptive，降级为 extended with default budget
                    AnthropicThinkingMode::ExtendedThinking {
                        budget_tokens: self.thinking_budget,
                    }
                }
            }

            abacus_types::ThinkingIntent::Effort(level) => {
                if supports_adaptive {
                    AnthropicThinkingMode::Adaptive {
                        effort: anthropic_effort_str(level).into(),
                        display: None,
                    }
                } else if opus_4_7_or_mythos {
                    // 罕见：Opus 4.7 但 supports_adaptive 错判 → 仍走 adaptive 兜底
                    AnthropicThinkingMode::Adaptive {
                        effort: anthropic_effort_str(level).into(),
                        display: None,
                    }
                } else {
                    // 旧模型：根据档位转 budget
                    AnthropicThinkingMode::ExtendedThinking {
                        budget_tokens: level.default_budget_tokens(),
                    }
                }
            }

            abacus_types::ThinkingIntent::Budget(n) => {
                // 显式预算永远走 extended（即使 4.7 不支持—— Opus 4.7 上 budget 会降级为 adaptive max）
                if opus_4_7_or_mythos {
                    AnthropicThinkingMode::Adaptive {
                        effort: "max".into(),
                        display: None,
                    }
                } else {
                    AnthropicThinkingMode::ExtendedThinking { budget_tokens: n }
                }
            }
        }
    }

    fn build_messages(&self, req: &LlmRequest) -> Vec<AnthropicMessage> {
        let mut out = Vec::new();

        for msg in &req.messages {
            match msg.role {
                MessageRole::System => {
                    // Anthropic: system prompt 是顶层字段，不在 messages 中
                    // 如果 req.system 已设置，跳过避免重复；否则转为 user 消息兜底
                    if req.system.is_some() {
                        continue;
                    }
                    let text = extract_text_content(msg);
                    out.push(AnthropicMessage {
                        role: "user".into(),
                        content: AnthropicContent::Text(text),
                    });
                }
                MessageRole::User => {
                    let content = match &msg.content {
                        Some(MessageContent::Text(t)) => {
                            if let Some(ref calls) = msg.tool_calls {
                                // User message with tool_calls → tool_result content blocks
                                let mut blocks: Vec<ContentBlock> = Vec::new();
                                if !t.is_empty() {
                                    blocks.push(ContentBlock::Text { text: t.clone() });
                                }
                                for tc in calls {
                                    blocks.push(ContentBlock::ToolResult {
                                        tool_use_id: tc.id.clone(),
                                        content: serde_json::Value::String(tc.function.arguments.clone()),
                                    });
                                }
                                AnthropicContent::Blocks(blocks)
                            } else {
                                AnthropicContent::Text(t.clone())
                            }
                        }
                        Some(MessageContent::MultiPart(parts)) => {
                            // MultiPart → content blocks（Anthropic 格式）
                            let mut blocks = Vec::new();
                            for part in parts {
                                match part {
                                    crate::llm::provider::ContentPart::Text { text } => {
                                        blocks.push(ContentBlock::Text { text: text.clone() });
                                    }
                                    crate::llm::provider::ContentPart::ImageUrl { image_url } => {
                                        if let Some((media_type, data)) = parse_data_url(&image_url.url) {
                                            blocks.push(ContentBlock::Image {
                                                source: ImageSource {
                                                    type_: "base64".into(),
                                                    media_type,
                                                    data,
                                                },
                                            });
                                        } else {
                                            warn!(
                                                "Anthropic only supports base64 data URLs for images, got: {}",
                                                &image_url.url[..image_url.url.len().min(80)]
                                            );
                                        }
                                    }
                                    crate::llm::provider::ContentPart::ToolResult { tool_use_id, content } => {
                                        blocks.push(ContentBlock::ToolResult {
                                            tool_use_id: tool_use_id.clone(),
                                            content: serde_json::Value::String(content.clone()),
                                        });
                                    }
                                    crate::llm::provider::ContentPart::ToolUse { .. } => {
                                        // ToolUse in user messages should not occur;
                                        // Anthropic handles tool_use only in assistant messages
                                    }
                                }
                            }
                            AnthropicContent::Blocks(blocks)
                        }
                        None => AnthropicContent::Text(String::new()),
                    };
                    out.push(AnthropicMessage { role: "user".into(), content });
                }
                MessageRole::Assistant => {
                    let text = extract_text_content(msg);
                    if let Some(ref calls) = msg.tool_calls {
                        let mut blocks: Vec<ContentBlock> = Vec::new();
                        if !text.is_empty() {
                            blocks.push(ContentBlock::Text { text });
                        }
                        for tc in calls {
                            let input: serde_json::Value =
                                serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                            blocks.push(ContentBlock::ToolUse {
                                id: tc.id.clone(),
                                name: tc.function.name.clone(),
                                input,
                            });
                        }
                        out.push(AnthropicMessage {
                            role: "assistant".into(),
                            content: AnthropicContent::Blocks(blocks),
                        });
                    } else if let Some(ref rc) = msg.reasoning_content {
                        // Reasoning content from previous turn → 合并到 text 中
                        // Anthropic 的 thinking block 是响应只读的，不能回传给 API
                        let combined = if text.is_empty() {
                            rc.clone()
                        } else {
                            format!("{}\n\n{}", rc, text)
                        };
                        out.push(AnthropicMessage {
                            role: "assistant".into(),
                            content: AnthropicContent::Text(combined),
                        });
                    } else {
                        out.push(AnthropicMessage {
                            role: "assistant".into(),
                            content: AnthropicContent::Text(text),
                        });
                    }
                }
                MessageRole::Tool => {
                    // Tool role messages → tool_result
                    // Anthropic 要求 tool_result 紧跟在发起 tool_use 的 assistant 消息之后
                    let text = extract_text_content(msg);
                    let tool_use_id = match &msg.tool_call_id {
                        Some(id) if !id.is_empty() => id.clone(),
                        _ => {
                            // 无 tool_call_id 时用 text 前 12 字符作为 fallback
                            let fallback = if text.len() > 12 { &text[..12] } else { &text };
                            warn!("Anthropic tool_result missing tool_call_id, using fallback: {fallback}");
                            format!("fallback_{}", fallback)
                        }
                    };
                    let mut blocks = Vec::new();
                    if !text.is_empty() {
                        blocks.push(ContentBlock::Text { text: text.clone() });
                    }
                    blocks.push(ContentBlock::ToolResult {
                        tool_use_id,
                        content: serde_json::Value::String(text),
                    });
                    out.push(AnthropicMessage {
                        role: "user".into(),
                        content: AnthropicContent::Blocks(blocks),
                    });
                }
            }
        }

        // Phase 4 KV cache：把 user_message_preamble 拼到最后一条 role="user" 消息顶部
        //   不影响前缀 cache（user message 永不缓存），且 LLM 把 preamble 视为本轮检索素材
        if let Some(ref preamble) = req.user_message_preamble {
            if let Some(last_user) = out.iter_mut().rev().find(|m| m.role == "user") {
                match &mut last_user.content {
                    AnthropicContent::Text(t) => {
                        *t = format!("{}\n\n{}", preamble, t);
                    }
                    AnthropicContent::Blocks(blocks) => {
                        // 在第一个 Text block 前面插入 preamble；如无 Text block 则新插一个
                        let first_text_idx = blocks.iter().position(|b| matches!(b, ContentBlock::Text { .. }));
                        match first_text_idx {
                            Some(idx) => {
                                if let ContentBlock::Text { text } = &mut blocks[idx] {
                                    *text = format!("{}\n\n{}", preamble, text);
                                }
                            }
                            None => {
                                blocks.insert(0, ContentBlock::Text { text: preamble.clone() });
                            }
                        }
                    }
                }
            }
        }

        out
    }

    fn parse_response(&self, raw: AnthropicResponse) -> Result<LlmResponse> {
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut thinking_text: Option<String> = None;

        for block in raw.content {
            match block {
                ResponseBlock::Text { text: t, .. } => {
                    text.push_str(&t);
                }
                ResponseBlock::ToolUse { id, name, input } => {
                    tool_calls.push(crate::llm::provider::ToolCall {
                        id,
                        type_: "function".into(),
                        function: crate::llm::provider::ToolFunction {
                            name,
                            arguments: serde_json::to_string(&input).unwrap_or_default(),
                        },
                    });
                }
                ResponseBlock::Thinking { thinking: t, .. } => {
                    thinking_text = Some(t);
                }
                ResponseBlock::RedactedThinking { .. } => {
                    thinking_text = Some("[redacted thinking]".into());
                }
            }
        }

        let finish_reason = match raw.stop_reason.as_str() {
            "end_turn" | "stop_sequence" | "stop" => "stop".into(),
            "tool_use" => "tool_calls".into(),
            "max_tokens" => "length".into(),
            other => other.into(),
        };

        let msg = Message {
            role: MessageRole::Assistant,
            content: Some(MessageContent::Text(text)),
            name: None,
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            tool_call_id: None,
            reasoning_content: thinking_text.clone(),
            prefix: false,
        };

        let model = if raw.model.is_empty() {
            self.model.clone()
        } else {
            ModelId(raw.model)
        };

        let input_tokens = raw.usage.input_tokens;
        let output_tokens = raw.usage.output_tokens;
        // V30 语义规范化：Anthropic API 把 input/cache_read/cache_creation 拆三段，
        // 必须合并后填 prompt_tokens 才符合 TokenUsage 跨 provider 不变量
        // （cached_tokens / cache_creation_tokens 是 prompt_tokens 的子集）。
        // 修复前 prompt_tokens=input 不含 cached → cost 公式 saturating_sub 失真，
        // 命中率显示 >100% 被强压 100%（详见 components/mod.rs:3525）。
        let cache_read = raw.usage.cache_read_input_tokens.unwrap_or(0);
        let cache_creation = raw.usage.cache_creation_input_tokens.unwrap_or(0);
        let prompt_tokens = input_tokens + cache_read + cache_creation;

        Ok(LlmResponse {
            model,
            message: msg,
            finish_reason,
            usage: TokenUsage {
                prompt_tokens,
                completion_tokens: output_tokens,
                total_tokens: prompt_tokens + output_tokens,
                cached_tokens: cache_read,
                cache_creation_tokens: cache_creation,
                // Anthropic API 不返回独立 thinking token 计数，extended thinking 的
                // 字节计入 output_tokens；置 0 表示"无独立字段曝露"，TUI 不显示思考行
                thinking_tokens: 0,
            },
            thinking: thinking_text,
            cache_stats: Some(CacheStats {
                cache_creation_tokens: raw.usage.cache_creation_input_tokens.unwrap_or(0),
                cache_read_tokens: raw.usage.cache_read_input_tokens.unwrap_or(0),
            }),
        })
    }
}

// ── LlmProvider trait 实现 ─────────────────────────────────────────────

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse> {
        let body = self.build_request(&req);

        debug!(
            model = %body.model,
            messages = %body.messages.len(),
            tools = %body.tools.as_ref().map_or(0, |t| t.len()),
            "Anthropic messages request"
        );

        let mut retries: u64 = 0;
        let max_retries = 5;

        let resp = loop {
            let mut req_builder = self
                .client
                .post(self.messages_url())
                .timeout(self.request_timeout) // H8: per-request timeout 替代 Client.builder.timeout
                .header("x-api-key", self.api_key.as_str())
                .header("anthropic-version", Self::API_VERSION)
                .header("Content-Type", "application/json");
            for (name, value) in &self.beta_headers {
                req_builder = req_builder.header(name, value);
            }
            let result = req_builder
                .json(&body)
                .send()
                .await;

            let resp = match result {
                Ok(r) => r,
                Err(e) => {
                    if retries < max_retries {
                        retries += 1;
                        let delay = Duration::from_secs(retries);
                        tracing::info!(
                            attempt = retries,
                            max = max_retries,
                            error = %e,
                            "anthropic: retrying after transport error"
                        );
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
            return Err(KernelError::ApiError {
                status: status.as_u16(),
                body: body_text,
            });
        }

        let raw: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| KernelError::Provider(format!("response parse failed: {e}")))?;

        // Anthropic API may return error type responses with a top-level error object.
        // e.g. {"type": "error", "error": {"type": "overloaded_error", "message": "..."}}
        if raw.get("type").and_then(|t| t.as_str()) == Some("error") {
            let err_msg = raw.pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown anthropic API error");
            let err_type = raw.pointer("/error/type")
                .and_then(|t| t.as_str())
                .unwrap_or("unknown");
            return Err(KernelError::ApiError {
                status: 0,
                body: format!("anthropic {err_type}: {err_msg}"),
            });
        }

        let parsed: AnthropicResponse = serde_json::from_value(raw)
            .map_err(|e| KernelError::Provider(format!("response parse failed: {e}")))?;

        debug!(
            model = %parsed.model,
            usage = ?parsed.usage,
            "Anthropic messages response"
        );

        self.parse_response(parsed)
    }

    /// V0.2: Anthropic SSE streaming — event types: content_block_delta, message_delta
    async fn stream_complete(
        &self,
        req: LlmRequest,
        tx: tokio::sync::mpsc::UnboundedSender<crate::llm::stream::StreamEvent>,
    ) -> Result<LlmResponse> {
        use crate::llm::stream::StreamEvent;
        use futures_util::StreamExt;

        let mut body = self.build_request(&req);
        body.stream = true;

        let mut req_builder = self
            .client
            .post(self.messages_url())
            .timeout(self.request_timeout) // H8: per-request timeout
            .header("x-api-key", self.api_key.as_str())
            .header("anthropic-version", Self::API_VERSION)
            .header("Content-Type", "application/json");
        for (name, value) in &self.beta_headers {
            req_builder = req_builder.header(name, value);
        }

        let resp = req_builder.json(&body).send().await
            .map_err(|e| KernelError::Provider(format!("stream request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            let _ = tx.send(StreamEvent::Error(
                format!("HTTP {}: {}", status.as_u16(), &body_text[..body_text.len().min(200)])
            ));
            return Err(KernelError::ApiError { status: status.as_u16(), body: body_text });
        }

        let mut byte_stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut full_thinking = String::new();
        let mut prompt_tokens = 0u64;
        let mut completion_tokens = 0u64;

        // P2: stream idle timeout — 45s 无新 chunk 视为连接死锁，主动断开
        loop {
            let chunk = match tokio::time::timeout(Duration::from_secs(45), byte_stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break, // stream 正常结束
                Err(_) => {
                    tracing::warn!("stream idle timeout (45s), treating as complete");
                    let _ = tx.send(StreamEvent::Error("stream idle timeout (45s)".into()));
                    break;
                }
            };
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(
                        format!("stream interrupted: {e}")
                    ));
                    return Err(KernelError::Provider(format!("stream read: {e}")));
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if line.is_empty() || line.starts_with(':') { continue; }

                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" { break; }

                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

                        match event_type {
                            "content_block_delta" => {
                                if let Some(delta) = event.get("delta") {
                                    let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    match delta_type {
                                        "text_delta" => {
                                            if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                                full_text.push_str(text);
                                                let _ = tx.send(StreamEvent::TextDelta(text.to_string()));
                                            }
                                        }
                                        "thinking_delta" => {
                                            if let Some(thinking) = delta.get("thinking").and_then(|t| t.as_str()) {
                                                full_thinking.push_str(thinking);
                                                let _ = tx.send(StreamEvent::ThinkingDelta(thinking.to_string()));
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            "message_delta" => {
                                if let Some(usage) = event.get("usage") {
                                    completion_tokens = usage.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                                }
                            }
                            "message_start" => {
                                if let Some(usage) = event.pointer("/message/usage") {
                                    prompt_tokens = usage.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                                }
                            }
                            "message_stop" => {
                                break;
                            }
                            _ => {}
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
            finish_reason: "end_turn".to_string(),
            usage: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                cached_tokens: 0,
                cache_creation_tokens: 0,
                // Anthropic 流式当前未拆 cache 字段，extended thinking 计入 completion；
                // thinking_tokens=0 与非流式路径对齐
                thinking_tokens: 0,
            },
            thinking: if full_thinking.is_empty() { None } else { Some(full_thinking) },
            cache_stats: None,
        })
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

    fn provider_id(&self) -> &str {
        "anthropic"
    }

    fn supported_models(&self) -> Vec<ModelId> {
        vec![self.model.clone()]
    }

    /// 2026-05-28: 调用 Anthropic /v1/models API 发现所有可用模型
    ///
    /// ## 引用关系
    /// - 调用方: CoreLoop::discover_all_models()
    /// - 依赖: Anthropic Models API (GET /v1/models, x-api-key header)
    async fn discover_models(&self) -> abacus_types::Result<Vec<ModelId>> {
        // 只从用户配置的 URL 检测；未配置时返回静态列表，不发网络请求
        if !self.discover_enabled {
            return Ok(self.supported_models());
        }
        let resp = crate::llm::shared_http_client()
            .get(format!("{}/v1/models", self.base_url))
            .timeout(std::time::Duration::from_secs(15))
            .header("x-api-key", self.api_key.as_str())
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| abacus_types::KernelError::Provider(format!("discover models: {e}")))?;
        if !resp.status().is_success() {
            // Anthropic 可能不支持 /v1/models（旧版 API）→ 回退到内置列表
            tracing::debug!(status = resp.status().as_u16(), "Anthropic /v1/models not available, fallback");
            return Ok(self.supported_models());
        }
        #[derive(serde::Deserialize)]
        struct ModelEntry { id: String }
        #[derive(serde::Deserialize)]
        struct ModelList { data: Vec<ModelEntry> }
        match resp.json::<ModelList>().await {
            Ok(parsed) => Ok(parsed.data.into_iter().map(|e| ModelId(e.id)).collect()),
            Err(_) => Ok(self.supported_models()), // 解析失败回退
        }
    }
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("beta_headers", &self.beta_headers)
            .field("thinking_budget", &self.thinking_budget)
            .finish()
    }
}

// ── 辅助 ───────────────────────────────────────────────────────────────

fn extract_text_content(msg: &Message) -> String {
    match &msg.content {
        Some(MessageContent::Text(t)) => t.clone(),
        Some(MessageContent::MultiPart(parts)) => {
            let mut text = String::new();
            for part in parts {
                if let crate::llm::provider::ContentPart::Text { text: t } = part {
                    text.push_str(t);
                }
            }
            text
        }
        None => String::new(),
    }
}

/// 解析 `data:image/{format};base64,{data}` 格式的 data URL
/// 返回 (media_type, base64_data) 或 None
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    if !url.starts_with("data:") {
        return None;
    }
    let rest = url.strip_prefix("data:")?;
    let (media_part, data) = rest.split_once(',')?;
    // media_part 格式: "image/jpeg;base64" 或 "image/png;base64"
    if !media_part.ends_with(";base64") {
        return None;
    }
    let media_type = media_part.strip_suffix(";base64")?.to_string();
    Some((media_type, data.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_data_url_valid() {
        let url = "data:image/jpeg;base64,/9j/4AAQSkZJRg==";
        let (media_type, data) = parse_data_url(url).unwrap();
        assert_eq!(media_type, "image/jpeg");
        assert_eq!(data, "/9j/4AAQSkZJRg==");
    }

    #[test]
    fn test_parse_data_url_png() {
        let url = "data:image/png;base64,iVBORw0KGgo=";
        let (media_type, data) = parse_data_url(url).unwrap();
        assert_eq!(media_type, "image/png");
        assert_eq!(data, "iVBORw0KGgo=");
    }

    #[test]
    fn test_parse_data_url_invalid() {
        assert!(parse_data_url("https://example.com/image.jpg").is_none());
        assert!(parse_data_url("data:text/plain,hello").is_none());
        assert!(parse_data_url("").is_none());
    }

    #[test]
    fn test_reasoning_content_in_response_sets_thinking() {
        // 模拟带 thinking 的响应:
        // content = [{type:"text",text:"answer"}]
        // 注意: thinking blocks 在响应中先出现，text 在后
        let json = serde_json::json!({
            "id": "msg_01",
            "type": "message",
            "content": [
                {"type": "thinking", "thinking": "I need to reason step by step..."},
                {"type": "text", "text": "Here is the answer."}
            ],
            "stop_reason": "end_turn",
            "model": "claude-sonnet-4",
            "usage": {"input_tokens": 10, "output_tokens": 20}
        });
        let raw: AnthropicResponse = serde_json::from_value(json).unwrap();
        let provider = AnthropicProvider::new(
            "sk-test".into(), "claude-sonnet-4", None, None,
        );
        let result = provider.parse_response(raw).unwrap();
        assert!(result.thinking.is_some());
        assert_eq!(result.thinking.as_deref(), Some("I need to reason step by step..."));
        assert!(result.message.content.is_some());
    }

    #[test]
    fn test_tool_use_in_response() {
        let json = serde_json::json!({
            "id": "msg_02",
            "type": "message",
            "content": [
                {"type": "text", "text": "Let me check that."},
                {"type": "tool_use", "id": "tu_01", "name": "get_weather", "input": {"city": "Tokyo"}}
            ],
            "stop_reason": "tool_use",
            "model": "claude-sonnet-4",
            "usage": {"input_tokens": 10, "output_tokens": 25}
        });
        let raw: AnthropicResponse = serde_json::from_value(json).unwrap();
        let provider = AnthropicProvider::new(
            "sk-test".into(), "claude-sonnet-4", None, None,
        );
        let result = provider.parse_response(raw).unwrap();
        assert_eq!(result.finish_reason, "tool_calls");
        let calls = result.message.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "tu_01");
        assert_eq!(calls[0].function.name, "get_weather");
    }

    #[test]
    fn test_parse_data_url_webp() {
        let url = "data:image/webp;base64,UklGRhoA";
        let (media_type, data) = parse_data_url(url).unwrap();
        assert_eq!(media_type, "image/webp");
        assert_eq!(data, "UklGRhoA");
    }

    #[test]
    fn test_build_request_max_tokens_gt_budget_when_thinking() {
        let provider = AnthropicProvider::new(
            "sk-test".into(), "claude-sonnet-4", None, None,
        );
        let req = LlmRequest {
            model: ModelId("claude-sonnet-4".into()),
            messages: vec![],
            system: Some("Hello".into()),
            tools: vec![],
            temperature: Some(0.5),
            max_tokens: Some(3000),
            top_p: None,
            stop: vec![],
            stream: false,
            thinking_intent: Some(abacus_types::ThinkingIntent::Effort(
                abacus_types::EffortLevel::High,
            )),
            extra_body: HashMap::new(),
            cache_config: None,
            system_segments: Vec::new(),
            user_message_preamble: None,
        };
        let body = provider.build_request(&req);
        // budget=4096 for sonnet, max_tokens=3000 should be bumped to 4096+1024=5120
        assert!(body.max_tokens >= 5120);
    }

    // ── Phase 2 wire-format 测试 ──────────────────────────────────────────

    fn make_provider_for(model: &str) -> AnthropicProvider {
        AnthropicProvider::new("test-key".into(), ModelId(model.into()), None, None)
    }

    fn empty_req(model: &str) -> LlmRequest {
        LlmRequest {
            model: ModelId(model.into()),
            messages: vec![Message {
                role: MessageRole::User,
                content: Some(MessageContent::Text("hi".into())),
                name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
            }],
            system: None, system_segments: Vec::new(), tools: Vec::new(),
            temperature: None, max_tokens: Some(8192), top_p: None,
            stop: vec![], stream: false,
            thinking_intent: None,
            cache_config: None,
            extra_body: HashMap::new(),
            user_message_preamble: None,
        }
    }

    /// Phase 2 P0：Opus 4.7 + Adaptive intent → 必须发 thinking={type:"adaptive"} + output_config.effort
    /// 不能发 budget_tokens（API 拒收 400）
    #[test]
    fn test_opus_4_7_adaptive_wire_format() {
        let p = make_provider_for("claude-opus-4-7");
        let mut req = empty_req("claude-opus-4-7");
        req.thinking_intent = Some(abacus_types::ThinkingIntent::Adaptive);

        let body = p.build_request(&req);
        let json = serde_json::to_value(&body).expect("serialize");

        // 关键：thinking.type = "adaptive"，无 budget_tokens
        let thinking = json.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "adaptive");
        assert!(thinking.get("budget_tokens").is_none() || thinking["budget_tokens"].is_null(),
                "Opus 4.7 不能携带 budget_tokens（API 400）");

        // output_config.effort 必须存在
        let oc = json.get("output_config").expect("output_config field present");
        assert!(oc["effort"].is_string());
    }

    /// Phase 2 P0：Opus 4.7 + Effort(High) intent → 走 adaptive 路径，effort="high"
    #[test]
    fn test_opus_4_7_effort_high_maps_to_adaptive() {
        let p = make_provider_for("claude-opus-4-7");
        let mut req = empty_req("claude-opus-4-7");
        req.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::High,
        ));

        let body = p.build_request(&req);
        let json = serde_json::to_value(&body).unwrap();

        assert_eq!(json["thinking"]["type"], "adaptive");
        assert_eq!(json["output_config"]["effort"], "high");
    }

    /// Phase 2 P0：Opus 4.7 + Effort(XHigh) → effort="xhigh"（编码场景推荐）
    #[test]
    fn test_opus_4_7_xhigh_effort() {
        let p = make_provider_for("claude-opus-4-7");
        let mut req = empty_req("claude-opus-4-7");
        req.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::XHigh,
        ));

        let body = p.build_request(&req);
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["output_config"]["effort"], "xhigh");
    }

    /// Phase 2：Sonnet 4.5 旧模型 + Adaptive intent → 降级为 ExtendedThinking with budget_tokens
    /// 不能发 type:"adaptive"（旧模型不支持）
    #[test]
    fn test_sonnet_4_5_adaptive_falls_back_to_extended() {
        let p = make_provider_for("claude-sonnet-4-5");
        let mut req = empty_req("claude-sonnet-4-5");
        req.thinking_intent = Some(abacus_types::ThinkingIntent::Adaptive);

        let body = p.build_request(&req);
        let json = serde_json::to_value(&body).unwrap();

        let thinking = json.get("thinking").expect("thinking present");
        assert_eq!(thinking["type"], "enabled", "旧模型必须走 enabled 路径");
        assert!(thinking["budget_tokens"].is_u64(), "ExtendedThinking 必须含 budget_tokens");

        // 不能发 output_config（旧模型不识别）
        assert!(json.get("output_config").is_none() || json["output_config"].is_null());
    }

    /// Phase 2：Sonnet 4.6 双支持模型 + Adaptive → 走 adaptive 路径
    #[test]
    fn test_sonnet_4_6_adaptive_uses_new_path() {
        let p = make_provider_for("claude-sonnet-4-6");
        let mut req = empty_req("claude-sonnet-4-6");
        req.thinking_intent = Some(abacus_types::ThinkingIntent::Adaptive);

        let body = p.build_request(&req);
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["thinking"]["type"], "adaptive");
        assert!(json.get("output_config").is_some());
    }

    // L1 单通道清理：legacy `thinking` 字段已删除，原 `test_legacy_thinking_lifts_to_intent_on_opus_4_7`
    // 测试目标（lift legacy → intent）已不复存在，删除该测试避免误导。

    /// Phase 2：Off intent 不发任何 thinking/output_config 字段
    #[test]
    fn test_off_intent_omits_all_thinking_fields() {
        let p = make_provider_for("claude-opus-4-7");
        let mut req = empty_req("claude-opus-4-7");
        req.thinking_intent = Some(abacus_types::ThinkingIntent::Off);

        let body = p.build_request(&req);
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("thinking").is_none() || json["thinking"].is_null());
        assert!(json.get("output_config").is_none() || json["output_config"].is_null());
    }

    // L1 单通道清理：legacy `thinking` 字段已删除，原 `test_phase4_intent_overrides_legacy_thinking`
    // 测试 thinking_intent vs legacy thinking 的优先级——但 legacy 已不存在，测试失去对照变量。删除。

    // ── Insta snapshot 矩阵：Anthropic ─────────────────────────────────────
    //
    // 这些 snapshot 锁定 wire-format。任何字段名/结构变化都会让 review 流程触发，
    // 必须人工通过 `cargo insta review` 确认与官方 API schema 对齐后再 accept。
    //
    // 4 个矩阵 cell（详见 plan §6）：
    //   1. claude-opus-4-7 + Adaptive   → thinking.type=adaptive + output_config
    //   2. claude-opus-4-7 + Off        → 无 thinking / output_config
    //   3. claude-sonnet-4-6 + High     → thinking.type=enabled + budget_tokens
    //   4. claude-sonnet-4-5 + Adaptive → 降级 ExtendedThinking（旧模型不识别 adaptive）

    /// 把 snapshot 中不稳定字段（system 内容、messages）剥离，专注 wire-format 关键字段
    fn snapshot_relevant_fields(body: &AnthropicRequest) -> serde_json::Value {
        let json = serde_json::to_value(body).unwrap();
        serde_json::json!({
            "model": json["model"],
            "max_tokens": json["max_tokens"],
            "thinking": json["thinking"],
            "output_config": json["output_config"],
        })
    }

    #[test]
    fn snapshot_anthropic_opus_4_7_adaptive() {
        let p = make_provider_for("claude-opus-4-7");
        let mut req = empty_req("claude-opus-4-7");
        req.thinking_intent = Some(abacus_types::ThinkingIntent::Adaptive);

        let body = p.build_request(&req);
        insta::assert_json_snapshot!(snapshot_relevant_fields(&body));
    }

    #[test]
    fn snapshot_anthropic_opus_4_7_off() {
        let p = make_provider_for("claude-opus-4-7");
        let mut req = empty_req("claude-opus-4-7");
        req.thinking_intent = Some(abacus_types::ThinkingIntent::Off);

        let body = p.build_request(&req);
        insta::assert_json_snapshot!(snapshot_relevant_fields(&body));
    }

    #[test]
    fn snapshot_anthropic_sonnet_4_6_effort_high() {
        let p = make_provider_for("claude-sonnet-4-6");
        let mut req = empty_req("claude-sonnet-4-6");
        req.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::High,
        ));

        let body = p.build_request(&req);
        insta::assert_json_snapshot!(snapshot_relevant_fields(&body));
    }

    #[test]
    fn snapshot_anthropic_sonnet_4_5_adaptive_falls_back() {
        // Sonnet 4.5 不支持 adaptive → 必须降级为 ExtendedThinking
        let p = make_provider_for("claude-sonnet-4-5");
        let mut req = empty_req("claude-sonnet-4-5");
        req.thinking_intent = Some(abacus_types::ThinkingIntent::Adaptive);

        let body = p.build_request(&req);
        insta::assert_json_snapshot!(snapshot_relevant_fields(&body));
    }

    #[test]
    fn snapshot_anthropic_opus_4_7_xhigh_coding() {
        // Opus 4.7 编码场景 xhigh effort——Anthropic Mem 0 验证场景
        let p = make_provider_for("claude-opus-4-7");
        let mut req = empty_req("claude-opus-4-7");
        req.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::XHigh,
        ));

        let body = p.build_request(&req);
        insta::assert_json_snapshot!(snapshot_relevant_fields(&body));
    }

    // ─── Phase 4 KV cache 回归测试：preamble 不污染 cacheable system 段 ──────────

    /// 关键 invariant：user_message_preamble 不得修改任何 cacheable=true 的 SystemSegment
    /// 否则 Anthropic block cache 失效（cache_control prefix 不再 byte-identical）
    #[test]
    fn preamble_preserves_anthropic_cacheable_segments() {
        use crate::llm::provider::SystemSegment;
        let p = make_provider_for("claude-sonnet-4");
        let mut req = empty_req("claude-sonnet-4");
        req.system_segments = vec![
            SystemSegment { text: "STABLE_KERNEL_607t".into(), cacheable: true },
            SystemSegment { text: "STRATEGY_LAYER".into(), cacheable: true },
            SystemSegment { text: "DYNAMIC_FOCUS".into(), cacheable: false },
        ];
        req.user_message_preamble = Some("ICL_RAG_HIT".into());

        let body = p.build_request(&req);
        // wire body 的 system 字段是 Vec<{text, cache_control}>
        let sys_arr = serde_json::to_value(&body.system)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .expect("system should serialize as array of segments");

        // cacheable 段的 text 必须 byte-identical
        let kernel_text = sys_arr.iter().find_map(|seg| {
            seg.get("text").and_then(|t| t.as_str())
                .filter(|t| t.contains("STABLE_KERNEL_607t"))
        }).expect("STABLE_KERNEL_607t segment must exist");
        assert!(!kernel_text.contains("ICL_RAG_HIT"),
            "preamble 不得污染 cacheable kernel segment：{}", kernel_text);

        // preamble 应在 messages 的最后一条 user message 中
        let messages_arr = serde_json::to_value(&body.messages)
            .ok().and_then(|v| v.as_array().cloned())
            .expect("messages array");
        let last_user = messages_arr.iter().rev()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .expect("last user message");
        let last_user_str = serde_json::to_string(last_user).unwrap_or_default();
        assert!(last_user_str.contains("ICL_RAG_HIT"),
            "preamble 应在最后一条 user message 中：{}", last_user_str);
    }
}
