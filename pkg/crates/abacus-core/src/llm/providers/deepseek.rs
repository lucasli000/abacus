use abacus_types::{KernelError, ModelId, Pricing, Result, lookup_pricing};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::debug;

use crate::llm::prompt_cache::{CachedSegment, CachedSegmentKind};
use crate::llm::provider::{
    CacheStats, LlmProvider, LlmRequest, LlmResponse, Message, MessageContent, MessageRole,
    TokenUsage,
};

// ── Secret string that zeros on drop ─────

/// A string that zeroes its contents on drop to reduce exposure in core dumps.
/// Uses `zeroize` crate to prevent compiler optimizations from eliding the zeroing.
struct SecretString(zeroize::Zeroizing<String>);

impl SecretString {
    fn new(s: String) -> Self { Self(zeroize::Zeroizing::new(s)) }
    fn as_str(&self) -> &str { &self.0 }
}

// ── DeepSeek effort 字符串规范化 ──────────────────────────────────────────
//
// 官方文档（https://api-docs.deepseek.com/guides/thinking_mode）:
//   In thinking mode, for compatibility, `low` and `medium` are mapped to `high`,
//   and `xhigh` is mapped to `max`.
//
// 即 server 端真实差异化档位仅 high / max。客户端 clamp 的目的：
// - **wire 与 server 行为一致**：监控/日志看到 effort=high 就是真的 high，不是 low alias 后变 high
// - **资源决策准确**：cost 估算、超时 bonus 等下游逻辑读 wire 字符串时不被误导
// - **配置文档化**：用户写 reasoning_effort=low 在配置里实际跑的是 high，clamp 让这条信息暴露在 wire 而不仅是 server 端
//
// `None` 在调用方等价"不发字段"——保留给 thinking 关闭场景。
fn deepseek_effort_clamp(raw: &str) -> Option<&'static str> {
    match raw.to_ascii_lowercase().as_str() {
        // 服务端 alias 规则的 client 侧镜像
        "low" | "medium" | "high" => Some("high"),
        "xhigh" | "max" => Some("max"),
        // minimal/adaptive/未识别值：DeepSeek 不支持这些字符串 → 走默认 high
        _ => Some("high"),
    }
}

// ── DeepSeek-specific API types ──────────────────────────────────────────

#[derive(Serialize)]
struct DeepSeekRequest {
    model: String,
    messages: Vec<DeepSeekMessage>,
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
    thinking: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDef>>,
    /// Function calling 模式控制：tools 非空时发送 "auto"，让 API 通过 tool_calls 字段返回调用
    /// 而非在 content 中输出 XML 格式的 <tool_calls>。
    /// 引用关系：build_request 根据 tools 是否存在设置此字段
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct DeepSeekMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// DeepSeek preserves reasoning across turns
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    /// V31: prefix completion 标记（DeepSeek 专属，仅 last assistant message 可设 true）
    /// 引用关系：业务层从 LlmRequest.messages[N].prefix 透传
    /// 默认 false → 序列化时跳过，wire 格式向后兼容
    #[serde(default, skip_serializing_if = "is_false_local")]
    prefix: bool,
}

/// 序列化辅助：prefix=false 时跳过字段输出
fn is_false_local(b: &bool) -> bool {
    !b
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
struct DeepSeekResponse {
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
    /// DeepSeek's reasoning content (separate from visible content)
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    /// V30 后改为 manual sum（API total 在 thinking 模式下与 prompt+completion 不一致）；
    /// 字段保留供未来诊断比对，因此 allow(dead_code)
    #[serde(default)]
    #[allow(dead_code)]
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// V30 透明度修复：DeepSeek-V4/Reasoner thinking 模式返回 reasoning_tokens（completion 子集）
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct CompletionTokensDetails {
    /// V30：思考 tokens（completion_tokens 子集，仅曝露用）
    #[serde(default)]
    reasoning_tokens: u64,
}

// ── DeepSeek Provider ─────────────────────────────────────────────────

pub struct DeepSeekProvider {
    client: Client,
    /// H8: per-request timeout（共享 Client 池）
    request_timeout: Duration,
    api_key: SecretString,
    base_url: String,
    model: ModelId,
    pricing: Pricing,
    reasoning_effort: Option<String>,
    default_max_tokens: u32,
    /// 是否允许 discover_models() 发网络请求
    /// true = 用户显式配置了 base_url → 允许打 /v1/models
    /// false = 使用内置默认 URL → 只返回静态列表
    /// 引用关系：with_config 根据 base_url 参数设置；discover_models() 消费
    discover_enabled: bool,
    /// Authorization 头前缀（默认 "Bearer "，与 openai_compatible 一致）
    auth_prefix: String,
}

impl DeepSeekProvider {
    const DEFAULT_BASE_URL: &'static str = "https://api.deepseek.com";
    /// V31: Beta endpoint —— prefix completion / FIM 等 Beta API 入口
    /// 引用关系：with_config 检测 model_registry::lookup_model.supports_prefix_completion
    /// 决定是否走 beta 域；beta 兼容主域所有功能，加多 Beta API
    const BETA_BASE_URL: &'static str = "https://api.deepseek.com/beta";
    const MODEL_FLASH: &'static str = "deepseek-v4-flash";
    const MODEL_PRO: &'static str = "deepseek-v4-pro";
    const DEFAULT_AUTH_PREFIX: &'static str = "Bearer ";

    /// Create a new DeepSeek provider.
    ///
    /// # Arguments
    /// * `api_key` - DeepSeek API key
    /// * `model` - Model name (e.g. "deepseek-v4-flash", "deepseek-v4-pro")
    /// * `base_url` - Optional custom base URL (default: `https: //api.deepseek.com`)
    pub fn new(api_key: String, model: impl Into<ModelId>) -> Self {
        Self::with_config(api_key, model, None, None, None, None)
    }

    /// Create with full configuration.
    ///
    /// `auth_prefix`: Authorization 头前缀。默认 `Bearer `。当用户使用自定义网关
    /// 且该网关要求 `ApiKey xxx` / `Token xxx` 等其他格式时传入。
    pub fn with_config(
        api_key: String,
        model: impl Into<ModelId>,
        base_url: Option<String>,
        reasoning_effort: Option<String>,
        timeout_secs: Option<u64>,
        auth_prefix: Option<String>,
    ) -> Self {
        // H8: 复用进程级共享 Client（pool/keepalive 配置已在 shared_http_client 中合并）
        let client = crate::llm::shared_http_client().clone();
        let request_timeout = Duration::from_secs(timeout_secs.unwrap_or(600));

        let model_id: ModelId = model.into();
        let model_str = model_id.0.as_str();

        // V28.7: Pricing 单一真相源 → abacus_types::lookup_pricing
        // 引用关系：cli/tui/cost.rs 也走同一函数，避免双源漂移
        let pricing = lookup_pricing(model_str);

        // V31: Beta endpoint 自动选择
        // 引用关系：abacus_types::lookup_model 查 model 能力
        // 规则：① 用户显式传 base_url → 优先（覆盖自动选）
        //       ② 否则查 model.supports_prefix_completion → true 用 BETA_BASE_URL
        //       ③ 否则用 DEFAULT_BASE_URL（主域，全功能）
        // 设计意图：V4-Flash/Pro 走 beta 自动启用 prefix；legacy alias 走主域兼容老路径
        let discover_enabled = base_url.is_some();
        let resolved_base_url = base_url.unwrap_or_else(|| {
            let supports_prefix = abacus_types::lookup_model(model_str)
                .map(|m| m.supports_prefix_completion)
                .unwrap_or(false);
            if supports_prefix {
                Self::BETA_BASE_URL.to_string()
            } else {
                Self::DEFAULT_BASE_URL.to_string()
            }
        });

        Self {
            client,
            request_timeout,
            api_key: SecretString::new(api_key),
            base_url: resolved_base_url,
            model: model_id,
            pricing,
            reasoning_effort,
            default_max_tokens: 64000,
            discover_enabled,
            auth_prefix: auth_prefix.unwrap_or_else(|| Self::DEFAULT_AUTH_PREFIX.to_string()),
        }
    }

    /// Return a reference to the pricing model.
    pub fn pricing(&self) -> &Pricing {
        &self.pricing
    }

    /// Build the HTTP request body from an LlmRequest.
    fn build_request(&self, req: &LlmRequest) -> DeepSeekRequest {
        // L1 后单通道：唯一意图来源是 req.thinking_intent。
        // 单一真相源——先算 effective_thinking，再传给 build_messages，避免 wire 字段与
        // build_messages 行为错位（V15-V17 协议要求字段+reasoning_content 一致存在/缺失）。
        let model_str_for_decision = req.model.0.as_str();
        let is_reasoning_model_pre = model_str_for_decision.contains("reasoner")
            || model_str_for_decision.contains("r1")
            || model_str_for_decision.contains("deepseek-v4")
            || model_str_for_decision.contains("deepseek-v3")
            || model_str_for_decision == "deepseek-chat"; // V3 官方 model ID
        let intent_ref = req.thinking_intent.as_ref();
        let effective_thinking_enabled = is_reasoning_model_pre
            && intent_ref.is_some_and(|i| i.is_enabled());

        let messages: Vec<DeepSeekMessage> = self.build_messages(req, effective_thinking_enabled);

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

        // L1 + D3：thinking 字段决策——根据 thinking_intent 状态 + V4 default-enabled 规则。
        // V3.1+/V4 服务端默认 thinking=enabled；客户端必须显式发 `{type: "disabled"}` 关闭，
        // 否则与 build_messages 中 effective_thinking_enabled=false 不一致 → 400。
        // R1/reasoner thinking 永远 on，发 disabled 会被拒——不在 default-enabled 修复范围。
        // 2026-06-04 修复：删除 has_tools 降级逻辑。
        // 官方文档明确说明 thinking mode 支持 tool calls：
        //   https://api-docs.deepseek.com/guides/thinking_mode
        //   400 错误的真正原因是多轮未回传 reasoning_content，而非 thinking+tool 同时存在。
        //   build_messages 中已有 reasoning_content 回传逻辑，不受影响。
        let is_v4_default_enabled_series = (model_str_for_decision.contains("deepseek-v4")
            || model_str_for_decision.contains("deepseek-v3")
            || model_str_for_decision == "deepseek-chat") // V3 官方 model ID
            && !model_str_for_decision.contains("reasoner")
            && !model_str_for_decision.contains("r1");
        let thinking = if is_reasoning_model_pre {
            match intent_ref {
                Some(i) if i.is_enabled() => {
                    // thinking 启用：正常开启（无论是否有 tools）
                    Some(serde_json::json!({"type": "enabled"}))
                }
                Some(_) => {
                    // intent 是 Off → 显式 disable
                    Some(serde_json::json!({"type": "disabled"}))
                }
                None if is_v4_default_enabled_series => {
                    // 用户未配置：V3.1+/V4 必须显式 disable，避免 client/server 协议错配
                    Some(serde_json::json!({"type": "disabled"}))
                }
                None => None, // reasoner / r1 / 自定义：保留旧行为不发字段
            }
        } else {
            None
        };
        // thinking 实际是否启用（驱动 build_messages 和 reasoning_effort）
        // 不再因 has_tools 降级——thinking 和 tools 可以同时发送。

        // D2: client-side clamp。官方文档规则：low/medium→high, xhigh→max。
        // 把规则前移到 client，让 wire 上发的字符串与 server 真实执行档位一致。
        // 仅在 thinking 实际启用 + reasoning model 时发送 effort 字段。
        let raw_effort: Option<String> = if effective_thinking_enabled {
            match intent_ref {
                Some(abacus_types::ThinkingIntent::Effort(level)) => Some(level.as_str().to_string()),
                Some(abacus_types::ThinkingIntent::Adaptive) => Some("high".into()),
                Some(abacus_types::ThinkingIntent::Budget(_)) => Some("high".into()), // budget 在 DS 无原生对应
                _ => self.reasoning_effort.clone(),
            }
        } else {
            None
        };
        let reasoning_effort = raw_effort.as_deref()
            .map(|s| deepseek_effort_clamp(s).unwrap_or("high").to_string());

        // tools 非空时发送 tool_choice="auto"，确保 API 通过 structured tool_calls 字段返回
        // 而非在 content 中以 XML <tool_calls> 格式输出（DeepSeek 缺少此字段时的回退行为）
        let tool_choice = if tools.is_some() {
            Some(serde_json::json!("auto"))
        } else {
            None
        };

        DeepSeekRequest {
            model: req.model.0.clone(),
            messages,
            temperature: req.temperature,
            max_tokens: req.max_tokens.or(Some(self.default_max_tokens)),
            top_p: req.top_p,
            stop: req.stop.clone(),
            stream: false,
            thinking,
            reasoning_effort,
            tools,
            tool_choice,
            extra: req.extra_body.clone(),
        }
    }

    fn build_messages(&self, req: &LlmRequest, thinking_enabled: bool) -> Vec<DeepSeekMessage> {
        let mut out = Vec::new();

        // V16：thinking_enabled 由 build_request 上游计算后传入，确保与 DeepSeekRequest.thinking
        // 字段是否实际下发**一致**——避免 build_request 不发 thinking 但 build_messages 传空
        // reasoning_content（或反向），从而触发 thinking 状态/字段不一致 400。

        // Inject system prompt as first message if present
        if let Some(ref sys) = req.system {
            out.push(DeepSeekMessage {
                role: "system".into(),
                content: Some(serde_json::Value::String(sys.clone())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                prefix: false,
            });
        }

        // Phase 4 KV cache：找到最后一条 user message 索引，准备拼接 user_message_preamble
        //   preamble 字节每轮变化（ICL RAG 结果），但 user message 永不缓存 → 不破前缀 cache
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
                    // Phase 4：仅在最后一条 user message 拼接 preamble
                    if Some(idx) == last_user_idx {
                        if let Some(ref preamble) = req.user_message_preamble {
                            return serde_json::Value::String(format!("{}\n\n{}", preamble, t));
                        }
                    }
                    serde_json::Value::String(t.clone())
                },
                MessageContent::MultiPart(parts) => {
                    // Phase 4 fix：MultiPart 分支也需要注入 preamble（多模态 user message 不应丢失 ICL hits）
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

            // V15 修复（接续 V14）：依据当前请求的 thinking_enabled 双向决策。
            //   - Assistant + thinking 启用：必传（缺失/空补空字符串），否则 400 (a)
            //   - Assistant + thinking 关闭：不传，避免 400 (b)
            //   - 非 Assistant 角色：永远不传
            // 注意：tool_call 多轮场景下，DeepSeek 可能在某些短响应里返回空
            // reasoning_content；此时仍需作为字段送回，让 API 看到"我已遵循协议"。
            let reasoning_content = if matches!(msg.role, MessageRole::Assistant) && thinking_enabled {
                Some(msg.reasoning_content.clone().unwrap_or_default())
            } else {
                None
            };
            // V31: 透传 prefix 字段到 wire layer
            // 仅当 model.supports_prefix_completion=true 时业务层会设此字段
            // 引用关系：DeepSeekProvider::with_model_str 已查 model_registry 决定 base_url 切 beta
            out.push(DeepSeekMessage {
                role: role.into(),
                content,
                name: msg.name.clone(),
                tool_calls,
                tool_call_id: msg.tool_call_id.clone(),
                reasoning_content,
                prefix: msg.prefix,
            });
        }

        out
    }

    /// Parse the DeepSeek API response into an LlmResponse.
    fn parse_response(&self, raw: DeepSeekResponse) -> Result<LlmResponse> {
        let choice = raw
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| KernelError::Provider("empty choices in response".into()))?;

        let content = choice.message.content.unwrap_or_default();
        let finish_reason = choice.finish_reason;

        // DeepSeek API 要求：assistant 消息的 reasoning_content 必须原样保留传回，
        // 即使为空字符串也不能 filter 掉——下一轮 build_messages 需要它存在。
        let reasoning = choice.message.reasoning_content.clone();

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
            reasoning_content: reasoning.clone(),
            prefix: false,
        };

        let usage = raw.usage.unwrap_or(Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            prompt_tokens_details: None,
            completion_tokens_details: None,
        });

        let cached = usage
            .prompt_tokens_details
            .as_ref()
            .map(|d| d.cached_tokens)
            .unwrap_or(0);
        // V30：DeepSeek-V4/Reasoner thinking 模式 reasoning_tokens 是 completion 子集
        let thinking_tokens = usage
            .completion_tokens_details
            .as_ref()
            .map(|d| d.reasoning_tokens)
            .unwrap_or(0);

        let model = if raw.model.is_empty() {
            self.model.clone()
        } else {
            ModelId(raw.model)
        };

        Ok(LlmResponse {
            model,
            message: msg,
            finish_reason,
            usage: TokenUsage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                // V30 不变量：total = prompt + completion（不再信任 API total，
                // thinking 模式下 API total 可能 != prompt+completion）
                total_tokens: usage.prompt_tokens + usage.completion_tokens,
                cached_tokens: cached,
                cache_creation_tokens: 0,
                thinking_tokens,
            },
            thinking: reasoning,
            cache_stats: Some(CacheStats {
                cache_creation_tokens: 0,
                cache_read_tokens: cached,
            }),
        })
    }
}

#[async_trait]
impl LlmProvider for DeepSeekProvider {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse> {
        let body = self.build_request(&req);

        debug!(
            model = %body.model,
            messages = %body.messages.len(),
            tools = %body.tools.as_ref().map_or(0, |t| t.len()),
            "DeepSeek completions request"
        );

        // V18 wire trace: 写入 per-pid 路径（避免并发覆盖 + 0600 权限）
        // 安全守卫: 仅在 debug build 时写入，防止 release build 泄漏 API key
        #[cfg(debug_assertions)]
        if let Ok(json) = serde_json::to_string_pretty(&body) {
            crate::llm::wire_trace::write_wire_trace("deepseek", &self.base_url, &json);
        }

        let mut retries: u64 = 0;
        let max_retries = 5;

        let resp = loop {
            let result = self
                .client
                .post(format!("{}/v1/chat/completions", self.base_url))
                .timeout(self.request_timeout) // H8: per-request timeout
                .header("Authorization", format!("{}{}", self.auth_prefix, self.api_key.as_str()))
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
                        tracing::info!(
                            attempt = retries,
                            max = max_retries,
                            error = %e,
                            "deepseek: retrying after transport error"
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
            // V18：把请求摘要附在错误消息里（complete 路径，非 streaming）
            let summary = body.messages.iter().enumerate()
                .map(|(i, m)| {
                    let rc_state = match &m.reasoning_content {
                        Some(s) if s.is_empty() => "rc=Some(\"\")",
                        Some(_) => "rc=Some(non-empty)",
                        None => "rc=None",
                    };
                    format!("  [{}] role={} {}", i, m.role, rc_state)
                })
                .collect::<Vec<_>>().join("\n");
            let thinking_state = if body.thinking.is_some() { "ON" } else { "OFF" };
            let wire_hint = if cfg!(debug_assertions) {
                format!("\n(完整 JSON 见 {})", crate::llm::wire_trace::wire_trace_path("deepseek").display())
            } else {
                String::new()
            };
            let enriched = format!(
                "{}\n--- REQ DUMP (V18 complete) ---\nthinking={}\nmessages:\n{}{}",
                body_text, thinking_state, summary, wire_hint
            );
            return Err(KernelError::ApiError {
                status: status.as_u16(),
                body: enriched,
            });
        }

        let raw: DeepSeekResponse = resp
            .json()
            .await
            .map_err(|e| KernelError::Provider(format!("response parse failed: {e}")))?;

        debug!(
            model = %raw.model,
            usage = ?raw.usage,
            "DeepSeek completions response"
        );

        self.parse_response(raw)
    }

    /// V0.2: 真实 SSE streaming — 逐 chunk 推送文本增量。
    ///
    /// 使用 reqwest bytes_stream 解析 SSE `data:` 行，
    /// 每行解析为 delta 并发送到 channel。
    /// 最终返回聚合的完整 LlmResponse（与 complete() 行为一致）。
    async fn stream_complete(
        &self,
        req: LlmRequest,
        tx: tokio::sync::mpsc::UnboundedSender<crate::llm::stream::StreamEvent>,
    ) -> Result<LlmResponse> {
        use crate::llm::stream::StreamEvent;
        use futures_util::StreamExt;

        let mut body = self.build_request(&req);
        body.stream = true;

        // V18 wire trace: per-pid 路径避免并发覆盖 + 0600 权限
        #[cfg(debug_assertions)]
        if let Ok(json) = serde_json::to_string_pretty(&body) {
            crate::llm::wire_trace::write_wire_trace("deepseek (streaming)", &self.base_url, &json);
        }

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .timeout(self.request_timeout) // H8: per-request timeout
            .header("Authorization", format!("Bearer {}", self.api_key.as_str()))
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
            // V18：把请求摘要也带进错误消息，便于 TUI 立刻看出协议错配
            let summary = body.messages.iter().enumerate()
                .map(|(i, m)| {
                    let rc_state = match &m.reasoning_content {
                        Some(s) if s.is_empty() => "rc=Some(\"\")",
                        Some(_) => "rc=Some(non-empty)",
                        None => "rc=None",
                    };
                    format!("  [{}] role={} {}", i, m.role, rc_state)
                })
                .collect::<Vec<_>>().join("\n");
            let thinking_state = if body.thinking.is_some() { "ON" } else { "OFF" };
            let wire_hint = if cfg!(debug_assertions) {
                format!("\n(完整 JSON 见 {})", crate::llm::wire_trace::wire_trace_path("deepseek").display())
            } else {
                String::new()
            };
            let enriched = format!(
                "{}\n--- REQ DUMP (V18) ---\nthinking={}\nmessages:\n{}{}",
                body_text, thinking_state, summary, wire_hint
            );
            return Err(KernelError::ApiError { status: status.as_u16(), body: enriched });
        }

        // Parse SSE stream
        let mut byte_stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut full_thinking = String::new();
        let mut prompt_tokens = 0u64;
        // 流式模式缓存命中统计——从最后一个 usage chunk 读取
        // DeepSeek API 在最终 chunk 的 `usage.prompt_tokens_details.cached_tokens` 中返回
        let mut cached_tokens_stream = 0u64;
        // V30：thinking 模式下 reasoning_tokens（completion 子集）
        let mut thinking_tokens_stream = 0u64;
        let mut completion_tokens = 0u64;
        // OpenAI/DeepSeek SSE tool call ID 映射：index → id
        // 根因：SSE 只在第一个 chunk 携带 id，后续 chunk 仅有 index
        // 修复：建立 index→id 映射，后续 chunk 通过 index 查找 id，避免 arguments 丢失
        let mut tc_index_to_id: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
        // ToolCallStart 去重守卫：避免少数代理/模型在多个 chunk 中重复下发 function.name
        // 导致 TUI 出现重复的 Running 条目
        let mut tc_index_started: std::collections::HashSet<u64> = std::collections::HashSet::new();

        // P2: stream idle timeout — 45s 无新 chunk 视为连接死锁，主动断开
        // 🟡#7 治本：stream_alive 标志 + per-token send 失败时跳出循环
        let mut stream_alive = true;
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
            if !stream_alive { break; }
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

            // Process complete lines
            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if line.is_empty() || line.starts_with(':') {
                    continue; // SSE comment or empty line
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        if tx.send(StreamEvent::Done).is_err() {
                            tracing::debug!("stream consumer gone before Done");
                        }
                        break;
                    }

                    // Parse delta JSON
                    if let Ok(chunk_json) = serde_json::from_str::<serde_json::Value>(data) {
                        if let Some(choices) = chunk_json.get("choices").and_then(|c| c.as_array()) {
                            for choice in choices {
                                let delta = choice.get("delta").unwrap_or(&serde_json::Value::Null);

                                // Text content delta
                                if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                                    if !content.is_empty() {
                                        full_text.push_str(content);
                                        // 🟡#7 治本：per-token 失败标记死亡
                                        if tx.send(StreamEvent::TextDelta(content.to_string())).is_err() {
                                            tracing::debug!("stream consumer gone mid-stream; will stop sending");
                                            stream_alive = false;
                                        }
                                    }
                                }

                                // Reasoning/thinking content delta
                                if let Some(reasoning) = delta.get("reasoning_content").and_then(|r| r.as_str()) {
                                    if !reasoning.is_empty() {
                                        full_thinking.push_str(reasoning);
                                        if tx.send(StreamEvent::ThinkingDelta(reasoning.to_string())).is_err() {
                                            stream_alive = false;
                                        }
                                    }
                                }

                                // Tool calls (start/delta)
                                // OpenAI/DeepSeek SSE 协议：id 只在 index 首次出现的 chunk 携带；
                                // 后续 argument delta chunk 仅有 index，需通过映射表还原 id。
                                if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                                    for tc in tool_calls {
                                        let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                                        // 首次出现时（携带 id）存入映射表
                                        if let Some(id_str) = tc.get("id").and_then(|i| i.as_str()) {
                                            if !id_str.is_empty() {
                                                tc_index_to_id.insert(index, id_str.to_string());
                                            }
                                        }
                                        // 从映射表获取 id（后续 chunk 无 id 时通过 index 查找）
                                        let id = tc_index_to_id.get(&index)
                                            .cloned()
                                            .unwrap_or_else(|| {
                                                tc.get("id").and_then(|i| i.as_str())
                                                    .unwrap_or("")
                                                    .to_string()
                                            });
                                        if let Some(func) = tc.get("function") {
                                            if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                                                // 展示并且每个 index 只发一次 ToolCallStart
                                                if tc_index_started.insert(index) {
                                                    let _ = tx.send(StreamEvent::ToolCallStart {
                                                        id: id.clone(),
                                                        name: name.to_string(),
                                                    });
                                                }
                                            }
                                            if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                                                if !args.is_empty() {
                                                    let _ = tx.send(StreamEvent::ToolCallArgDelta {
                                                        id,
                                                        delta: args.to_string(),
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Usage 包（在最终 chunk 中返回）——同时提取缓存命中 + 思考 token 数
                        if let Some(usage) = chunk_json.get("usage") {
                            prompt_tokens = usage.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                            completion_tokens = usage.get("completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                            // 提取 KV 缓存命中 token：DeepSeek 放在 prompt_tokens_details.cached_tokens
                            cached_tokens_stream = usage
                                .get("prompt_tokens_details")
                                .and_then(|d| d.get("cached_tokens"))
                                .and_then(|t| t.as_u64())
                                .unwrap_or(0);
                            // V30：thinking 模式下 reasoning_tokens 是 completion_tokens 子集
                            thinking_tokens_stream = usage
                                .get("completion_tokens_details")
                                .and_then(|d| d.get("reasoning_tokens"))
                                .and_then(|t| t.as_u64())
                                .unwrap_or(0);
                        }
                    }
                }
            }
            // 🟡#7 治本：parse 完一个 chunk 后若 stream 已死，不再读下一个
            if !stream_alive { break; }
        }

        if tx.send(StreamEvent::Usage { prompt_tokens, completion_tokens }).is_err() {
            tracing::debug!("stream consumer gone before Usage");
        }
        if tx.send(StreamEvent::Done).is_err() {
            tracing::debug!("stream consumer gone before final Done");
        }

        // Build aggregated LlmResponse
        // V14：reasoning_content 仅在真实非空时存入；空占位会被 build_messages 跳过传输
        // 这保证："endpoint 不真返回 thinking" 场景下，下一轮请求不会带空字段触发 400
        let reasoning_for_message = if full_thinking.is_empty() {
            None
        } else {
            Some(full_thinking.clone())
        };

        Ok(LlmResponse {
            model: ModelId(body.model),
            message: Message {
                role: MessageRole::Assistant,
                content: Some(MessageContent::Text(full_text)),
                tool_calls: None, // Tool calls in streaming mode need separate assembly
                name: None,
                tool_call_id: None,
                reasoning_content: reasoning_for_message,
                prefix: false,
            },
            finish_reason: "stop".to_string(),
            usage: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                cached_tokens: cached_tokens_stream,
                cache_creation_tokens: 0,
                // V30：流式末 chunk usage.completion_tokens_details.reasoning_tokens 已在
                // 上方解析路径与 cached 同路径提取
                thinking_tokens: thinking_tokens_stream,
            },
            thinking: if full_thinking.is_empty() { None } else { Some(full_thinking) },
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
        "deepseek"
    }

    fn supported_models(&self) -> Vec<ModelId> {
        vec![
            ModelId(Self::MODEL_FLASH.into()),
            ModelId(Self::MODEL_PRO.into()),
            ModelId("deepseek-chat".into()),
            ModelId("deepseek-reasoner".into()),
        ]
    }

    /// GET {base_url}/v1/models — DeepSeek 是 OpenAI-compatible 协议
    /// 只从用户配置的 URL 检测；未配置时返回静态列表
    async fn discover_models(&self) -> abacus_types::Result<Vec<ModelId>> {
        if !self.discover_enabled {
            return Ok(self.supported_models());
        }
        let resp = self.client
            .get(format!("{}/v1/models", self.base_url))
            .timeout(std::time::Duration::from_secs(15))
            .header("Authorization", format!("Bearer {}", self.api_key.as_str()))
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

impl std::fmt::Debug for DeepSeekProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeepSeekProvider")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::provider::{ToolDefinition, ToolFunctionSpec};

    fn make_provider() -> DeepSeekProvider {
        DeepSeekProvider::new("test-key".into(), ModelId("deepseek-v4-flash".into()))
    }

    fn basic_req() -> LlmRequest {
        LlmRequest {
            model: ModelId("deepseek-v4-flash".into()),
            messages: vec![Message {
                role: MessageRole::User,
                content: Some(MessageContent::Text("hello".into())),
                name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
            }],
            system: Some("You are helpful.".into()),
            tools: vec![],
            temperature: Some(0.7),
            max_tokens: Some(100),
            top_p: None, stop: vec![], stream: false,
            thinking_intent: None, extra_body: Default::default(),
            cache_config: None, system_segments: vec![],
            user_message_preamble: None,
        }
    }

    #[test]
    fn test_build_request_basic_fields() {
        let p = make_provider();
        let req = p.build_request(&basic_req());
        assert_eq!(req.model, "deepseek-v4-flash");
        assert_eq!(req.temperature, Some(0.7));
        assert_eq!(req.max_tokens, Some(100));
        assert!(!req.stream);
        assert!(req.tools.is_none(), "no tools registered");
    }

    #[test]
    fn test_build_request_system_as_first_message() {
        let p = make_provider();
        let req = p.build_request(&basic_req());
        assert!(!req.messages.is_empty());
        assert_eq!(req.messages[0].role, "system");
        assert!(req.messages[0].content.as_ref().map(|c| c.as_str().unwrap_or("").contains("helpful")).unwrap_or(false));
    }

    #[test]
    fn test_build_request_with_tools() {
        let p = make_provider();
        let mut r = basic_req();
        r.tools = vec![ToolDefinition {
            type_: "function".into(),
            function: ToolFunctionSpec {
                name: "fs_read".into(),
                description: Some("read file".into()),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                strict: None,
            },
        }];
        let req = p.build_request(&r);
        let tools = req.tools.expect("tools should be set");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "fs_read");
        assert_eq!(tools[0].type_, "function");
    }

    /// D3 行为：V4 系列 thinking=None 在 wire 上必须显式发 `{type:"disabled"}`，
    /// 绕开服务端默认 enable 的协议错配。
    #[test]
    fn test_build_request_v4_default_disabled_explicit() {
        let p = make_provider(); // deepseek-v4-flash
        let req = p.build_request(&basic_req()); // thinking = None
        let thinking = req.thinking.as_ref().expect("D3: V4 系列必须显式发 thinking 字段");
        assert_eq!(
            thinking.get("type").and_then(|t| t.as_str()),
            Some("disabled"),
            "D3: V4 系列 thinking=None 时必须显式 disabled，不能省略字段"
        );
    }

    #[test]
    fn test_build_messages_tool_result_role() {
        let p = make_provider();
        let mut r = basic_req();
        r.messages.push(Message {
            role: MessageRole::Tool,
            content: Some(MessageContent::Text(r#"{"result": "ok"}"#.into())),
            name: None, tool_calls: None,
            tool_call_id: Some("call_abc".into()),
            reasoning_content: None,
            prefix: false,
        });
        let req = p.build_request(&r);
        let tool_msg = req.messages.iter().find(|m| m.role == "tool");
        assert!(tool_msg.is_some(), "should have tool role message");
        assert_eq!(tool_msg.unwrap().tool_call_id.as_deref(), Some("call_abc"));
    }

    #[test]
    fn test_build_messages_multi_turn() {
        // user → assistant(with tool_call) → tool_result → assistant
        let p = make_provider();
        let mut r = basic_req();
        r.messages.push(Message {
            role: MessageRole::Assistant,
            content: None,
            name: None,
            tool_calls: Some(vec![crate::llm::provider::ToolCall {
                id: "call_1".into(),
                type_: "function".into(),
                function: crate::llm::provider::ToolFunction {
                    name: "fs_read".into(),
                    arguments: r#"{"path": "/tmp/x"}"#.into(),
                },
            }]),
            tool_call_id: None,
            reasoning_content: None,
            prefix: false,
        });
        r.messages.push(Message {
            role: MessageRole::Tool,
            content: Some(MessageContent::Text("file content".into())),
            name: None, tool_calls: None,
            tool_call_id: Some("call_1".into()),
            reasoning_content: None,
            prefix: false,
        });
        let req = p.build_request(&r);
        // system + user + assistant + tool = 4 messages
        assert_eq!(req.messages.len(), 4);
        assert_eq!(req.messages[2].role, "assistant");
        assert!(req.messages[2].tool_calls.is_some());
        assert_eq!(req.messages[3].role, "tool");
    }

    /// 构造 reasoning 模型 provider（V16 起 effective_thinking_enabled 仅在
    /// model name 匹配 `reasoner|r1` 时为 true）
    fn make_reasoning_provider() -> DeepSeekProvider {
        DeepSeekProvider::new("test-key".into(), ModelId("deepseek-reasoner".into()))
    }

    fn reasoning_req() -> LlmRequest {
        let mut r = basic_req();
        r.model = ModelId("deepseek-reasoner".into());
        r
    }

    /// V15 回归测试：thinking 启用时，assistant 消息缺失 reasoning_content
    /// 必须以空字符串补齐字段，否则触发 DeepSeek 400
    /// "The reasoning_content in the thinking mode must be passed back to the API."
    #[test]
    fn test_build_messages_thinking_enabled_fills_missing_reasoning() {
        use abacus_types::{ThinkingIntent, EffortLevel};
        let p = make_reasoning_provider();
        let mut r = reasoning_req();
        r.thinking_intent = Some(ThinkingIntent::Effort(EffortLevel::High));
        // 历史 assistant turn 没有 reasoning_content（典型场景：tool_call 短回复）
        r.messages.push(Message {
            role: MessageRole::Assistant,
            content: Some(MessageContent::Text("calling tool".into())),
            name: None,
            tool_calls: Some(vec![crate::llm::provider::ToolCall {
                id: "call_1".into(),
                type_: "function".into(),
                function: crate::llm::provider::ToolFunction {
                    name: "fs_read".into(),
                    arguments: "{}".into(),
                },
            }]),
            tool_call_id: None,
            reasoning_content: None, // ← 关键：缺失
            prefix: false,
        });
        let req = p.build_request(&r);
        let assistant = req.messages.iter().find(|m| m.role == "assistant").expect("assistant message");
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some(""),
            "thinking 启用时 assistant.reasoning_content 必须存在（空串补齐）"
        );
    }

    /// V15 回归测试：thinking 启用时，assistant 已有非空 reasoning_content
    /// 必须原样回传
    #[test]
    fn test_build_messages_thinking_enabled_preserves_reasoning() {
        use abacus_types::{ThinkingIntent, EffortLevel};
        let p = make_reasoning_provider();
        let mut r = reasoning_req();
        r.thinking_intent = Some(ThinkingIntent::Effort(EffortLevel::Medium));
        r.messages.push(Message {
            role: MessageRole::Assistant,
            content: Some(MessageContent::Text("answer".into())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: Some("step 1: think; step 2: respond".into()),
            prefix: false,
        });
        let req = p.build_request(&r);
        let assistant = req.messages.iter().find(|m| m.role == "assistant").expect("assistant message");
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("step 1: think; step 2: respond"),
            "原样回传非空 reasoning_content"
        );
    }

    /// V17 回归测试：deepseek-v4-flash 也是 reasoning model（DeepSeek 官方 V3.1+/V4 全系支持
    /// thinking）。携带 ThinkingConfig::Enabled 时必须按协议回传 reasoning_content。
    /// 这是 V16 错误收紧白名单的反向测试——V17 恢复 v3/v4 在白名单内。
    #[test]
    fn test_build_messages_v17_v4_flash_is_reasoning_model() {
        use abacus_types::{ThinkingIntent, EffortLevel};
        let p = make_provider(); // deepseek-v4-flash
        let mut r = basic_req();
        r.thinking_intent = Some(ThinkingIntent::Effort(EffortLevel::Medium));
        r.messages.push(Message {
            role: MessageRole::Assistant,
            content: Some(MessageContent::Text("answer".into())),
            name: None, tool_calls: None, tool_call_id: None,
            reasoning_content: Some("prior reasoning trace".into()),
            prefix: false,
        });
        let req = p.build_request(&r);
        let assistant = req.messages.iter().find(|m| m.role == "assistant").expect("assistant message");
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("prior reasoning trace"),
            "V17: v4-flash 是 reasoning model；thinking enabled 时必须原样回传 reasoning_content"
        );
        // 同时验证 thinking 字段被发送
        assert!(req.thinking.is_some(), "V17: v4-flash 应当发送 thinking 字段");
    }

    /// D3 边界：reasoner / r1 模型 thinking=None 不发字段（避免显式 disabled 被拒）
    #[test]
    fn test_build_request_reasoner_no_thinking_omits_field() {
        let p = make_reasoning_provider(); // deepseek-reasoner
        let mut r = reasoning_req();
        r.thinking_intent = None;
        let req = p.build_request(&r);
        assert!(
            req.thinking.is_none(),
            "D3: reasoner thinking=None 不应发字段（thinking 永远 on，发 disabled 会被拒）"
        );
    }

    /// D3 配套：thinking=Some(Disabled) 也必须显式落到 wire 上
    #[test]
    fn test_build_request_explicit_disabled_writes_wire() {
        use abacus_types::ThinkingIntent;
        let p = make_provider();
        let mut r = basic_req();
        r.thinking_intent = Some(ThinkingIntent::Off);
        let req = p.build_request(&r);
        let thinking = req.thinking.expect("D3: 显式 disabled 必须落 wire");
        assert_eq!(thinking.get("type").and_then(|t| t.as_str()), Some("disabled"));
    }

    /// V15 回归测试：thinking 关闭时，assistant 即使带 reasoning_content
    /// 也不能传，否则触发旧的 400 "字段存在但 thinking 标志不一致"
    #[test]
    fn test_build_messages_thinking_disabled_strips_reasoning() {
        let p = make_provider();
        let mut r = basic_req();
        r.thinking_intent = None; // ← thinking 关闭
        r.messages.push(Message {
            role: MessageRole::Assistant,
            content: Some(MessageContent::Text("answer".into())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: Some("leftover thinking from prior turn".into()),
            prefix: false,
        });
        let req = p.build_request(&r);
        let assistant = req.messages.iter().find(|m| m.role == "assistant").expect("assistant message");
        assert!(
            assistant.reasoning_content.is_none(),
            "thinking 关闭时不能携带 reasoning_content（即使历史里有）"
        );
    }

    // ── Insta snapshot 矩阵：DeepSeek ──────────────────────────────────────

    fn snapshot_deepseek_fields(body: &DeepSeekRequest) -> serde_json::Value {
        let json = serde_json::to_value(body).unwrap();
        serde_json::json!({
            "model": json["model"],
            "thinking": json["thinking"],
            "reasoning_effort": json["reasoning_effort"],
            "stream": json["stream"],
        })
    }

    #[test]
    fn snapshot_deepseek_v4_pro_effort_low() {
        // D2 行为锁定：用户输入 "low" 在 wire 上 clamp 为 "high"——与官方 server-side
        // alias 规则镜像（low/medium → high）。Snapshot 捕获 wire 真发字符串而不是用户输入。
        let p = DeepSeekProvider::new("test-key".into(), ModelId("deepseek-v4-pro".into()));
        let mut r = basic_req();
        r.model = ModelId("deepseek-v4-pro".into());
        r.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::Low,
        ));

        let body = p.build_request(&r);
        insta::assert_json_snapshot!(snapshot_deepseek_fields(&body));
    }

    #[test]
    fn snapshot_deepseek_reasoner_effort_high() {
        let p = DeepSeekProvider::new("test-key".into(), ModelId("deepseek-reasoner".into()));
        let mut r = basic_req();
        r.model = ModelId("deepseek-reasoner".into());
        r.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
            abacus_types::EffortLevel::High,
        ));

        let body = p.build_request(&r);
        insta::assert_json_snapshot!(snapshot_deepseek_fields(&body));
    }

    #[test]
    fn snapshot_deepseek_v4_flash_thinking_disabled() {
        // D3: V4 系列 thinking=None 时 wire 必须显式发 `{type:"disabled"}`，
        // 绕开服务端默认 enable。Snapshot 锁定显式 disabled 不退化回 omit。
        let p = DeepSeekProvider::new("test-key".into(), ModelId("deepseek-v4-flash".into()));
        let mut r = basic_req();
        r.model = ModelId("deepseek-v4-flash".into());
        r.thinking_intent = None;

        let body = p.build_request(&r);
        insta::assert_json_snapshot!(snapshot_deepseek_fields(&body));
    }

    /// D2 单元测试：client-side clamp 把 low/medium → "high"，xhigh → "max"。
    /// 与官方 server-side alias 规则镜像一致，保证 wire 字符串与真实档位匹配。
    #[test]
    fn test_deepseek_effort_clamp_low_medium_to_high() {
        assert_eq!(deepseek_effort_clamp("low"), Some("high"));
        assert_eq!(deepseek_effort_clamp("Medium"), Some("high"));
        assert_eq!(deepseek_effort_clamp("HIGH"), Some("high"));
    }

    #[test]
    fn test_deepseek_effort_clamp_xhigh_to_max() {
        assert_eq!(deepseek_effort_clamp("xhigh"), Some("max"));
        assert_eq!(deepseek_effort_clamp("MAX"), Some("max"));
    }

    #[test]
    fn test_deepseek_effort_clamp_unknown_falls_back_high() {
        // minimal / adaptive / 自定义字符串 → high（保守默认）
        assert_eq!(deepseek_effort_clamp("minimal"), Some("high"));
        assert_eq!(deepseek_effort_clamp("adaptive"), Some("high"));
        assert_eq!(deepseek_effort_clamp("garbage"), Some("high"));
    }

    /// D2 端到端：build_request 路径上 effort=low 在 wire 上发 "high"
    #[test]
    fn test_build_request_clamps_low_to_high_on_wire() {
        use abacus_types::{ThinkingIntent, EffortLevel};
        let p = DeepSeekProvider::new("test-key".into(), ModelId("deepseek-v4-pro".into()));
        let mut r = basic_req();
        r.model = ModelId("deepseek-v4-pro".into());
        r.thinking_intent = Some(ThinkingIntent::Effort(EffortLevel::Low));
        let body = p.build_request(&r);
        assert_eq!(
            body.reasoning_effort.as_deref(),
            Some("high"),
            "D2: client-side clamp 让 'low' 在 wire 上变 'high'，与 server alias 一致"
        );
    }

    /// D2 边界：thinking 关闭时即使 self.reasoning_effort 有值，wire 上不发
    #[test]
    fn test_build_request_thinking_disabled_omits_effort() {
        let p = DeepSeekProvider::with_config(
            "test-key".into(),
            ModelId("deepseek-v4-pro".into()),
            None,
            Some("high".into()), // provider-level default
            None,
            None,  // auth_prefix: default "Bearer "
        );
        let mut r = basic_req();
        r.model = ModelId("deepseek-v4-pro".into());
        r.thinking_intent = None; // thinking 关
        let body = p.build_request(&r);
        assert!(
            body.reasoning_effort.is_none(),
            "thinking 关闭时不应发送 reasoning_effort（D2: 与 effective_thinking_enabled 守卫一致）"
        );
    }

    /// V15 回归测试：tool 角色消息从不携带 reasoning_content（无论 thinking 开关）
    #[test]
    fn test_build_messages_tool_role_never_has_reasoning() {
        use abacus_types::{ThinkingIntent, EffortLevel};
        let p = make_provider();
        let mut r = basic_req();
        r.thinking_intent = Some(ThinkingIntent::Effort(EffortLevel::Medium));
        r.messages.push(Message {
            role: MessageRole::Tool,
            content: Some(MessageContent::Text("result".into())),
            name: None,
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            reasoning_content: Some("should not appear".into()),
            prefix: false,
        });
        let req = p.build_request(&r);
        let tool_msg = req.messages.iter().find(|m| m.role == "tool").expect("tool message");
        assert!(tool_msg.reasoning_content.is_none(), "tool 角色不能携带 reasoning_content");
    }

    // ─── Phase 4 KV cache 回归测试：user_message_preamble 路径守门 ───────────────

    /// preamble 必须落在 user message，绝不污染 system 段（否则破前缀 cache）
    #[test]
    fn preamble_routes_to_user_message_not_system() {
        let p = make_provider();
        let mut r = basic_req();
        r.user_message_preamble = Some("## [Retrieved KB Context]\nSAFETY_RULE_42".into());
        let req = p.build_request(&r);

        // system 段不包含 preamble 字面值
        let sys_msg = req.messages.iter().find(|m| m.role == "system").expect("system message");
        let sys_text = sys_msg.content.as_ref().and_then(|v| v.as_str()).unwrap_or("");
        assert!(!sys_text.contains("Retrieved KB Context"),
            "preamble 不应注入 system 段（破坏前缀 cache）：{}", sys_text);

        // user 段包含 preamble
        let user_msg = req.messages.iter().rev().find(|m| m.role == "user").expect("user message");
        let user_text = user_msg.content.as_ref().and_then(|v| v.as_str()).unwrap_or("");
        assert!(user_text.contains("Retrieved KB Context") && user_text.contains("hello"),
            "preamble + 原 user 内容必须都在 user 段：{}", user_text);
        // 顺序：preamble 在前
        assert!(user_text.starts_with("## [Retrieved KB Context]"),
            "preamble 必须在 user message 顶部：{}", user_text);
    }

    /// 多条 user message 时，preamble 只注入最后一条（保留对话历史前缀稳定）
    #[test]
    fn preamble_only_on_last_user_message() {
        let p = make_provider();
        let mut r = basic_req();
        r.messages = vec![
            Message {
                role: MessageRole::User,
                content: Some(MessageContent::Text("turn1 question".into())),
                name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
            },
            Message {
                role: MessageRole::Assistant,
                content: Some(MessageContent::Text("turn1 answer".into())),
                name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
            },
            Message {
                role: MessageRole::User,
                content: Some(MessageContent::Text("turn2 question".into())),
                name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
            },
        ];
        r.user_message_preamble = Some("ICL_HIT_TURN2".into());
        let req = p.build_request(&r);

        let user_msgs: Vec<_> = req.messages.iter().filter(|m| m.role == "user").collect();
        assert_eq!(user_msgs.len(), 2);

        let first_text = user_msgs[0].content.as_ref().and_then(|v| v.as_str()).unwrap_or("");
        assert!(!first_text.contains("ICL_HIT_TURN2"),
            "首轮 user message 不能被 preamble 污染（破坏历史前缀）：{}", first_text);
        assert_eq!(first_text, "turn1 question");

        let last_text = user_msgs[1].content.as_ref().and_then(|v| v.as_str()).unwrap_or("");
        assert!(last_text.starts_with("ICL_HIT_TURN2"),
            "末轮 user message 应被 preamble 前缀：{}", last_text);
    }

    /// MultiPart（多模态）user message 也必须接收 preamble，不能丢失
    #[test]
    fn preamble_injects_into_multipart_user_message() {
        use crate::llm::provider::{ContentPart, ImageUrlSource};
        let p = make_provider();
        let mut r = basic_req();
        r.messages = vec![Message {
            role: MessageRole::User,
            content: Some(MessageContent::MultiPart(vec![
                ContentPart::Text { text: "describe this".into() },
                ContentPart::ImageUrl { image_url: ImageUrlSource { url: "data:image/png;base64,AAAA".into(), detail: None } },
            ])),
            name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
        }];
        r.user_message_preamble = Some("MULTIPART_PREAMBLE".into());
        let req = p.build_request(&r);

        let user_msg = req.messages.iter().find(|m| m.role == "user").expect("user message");
        let arr = user_msg.content.as_ref().and_then(|v| v.as_array()).expect("multipart array");
        assert!(arr.len() >= 3, "preamble 应作为新 part 插入，原 2 part + 1 = 3");
        let first_text = arr[0].get("text").and_then(|v| v.as_str()).unwrap_or("");
        assert_eq!(first_text, "MULTIPART_PREAMBLE",
            "preamble 应作为首个 text part：{:?}", arr[0]);
    }
}