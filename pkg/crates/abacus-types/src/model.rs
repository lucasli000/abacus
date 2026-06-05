//! L0 types — model layer: ModelId, ProviderId, capabilities, specs, pricing, thinking config
//!
//! ## Dependencies
//! - `serde` (workspace): derive macros for serialization
//! - `serde_json` (workspace): not directly used here but required by consumers
//!
//! ## References
//! - Referenced by: `abacus-core/src/llm/provider.rs` (ModelId, ModelSpec)
//! - Referenced by: `abacus-core/src/capability/mod.rs` (CapabilityKind::LlmCompletion)
//! - Referenced by: `abacus-core/src/core/mod.rs` (ModelId, CoreConfig, thinking fields)
//!
//! ## External Consumers
//! - `Pricing::input_cost` / `output_cost`: called by cost tracking in LLM provider responses
//!   (not yet implemented — placeholder for future billing module)

/// Opaque model identifier (e.g. "deepseek-v4-flash", "claude-sonnet-4-6")
///
/// ## AUTO sentinel
/// `ModelId::AUTO` ("auto") 表示"使用配置链中的默认模型"。调用方不需要知道具体模型名，
/// 由 `resolve_provider()` 按优先级链解析：
/// `model_override > ModelPreference.last_selected > ModelPreference.default > CoreConfig.default_model`
///
/// ## 引用关系
/// - CLI `--model` 参数默认值使用 `ModelId::AUTO`
/// - `engine_init.rs` / `server.rs` 解析时检测 `is_auto()` 后走配置链
/// - `ProviderRegistry::resolve()` 不接受 AUTO——调用方必须先解析
#[derive(Debug, Clone, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelId(pub String);

impl ModelId {
    /// Sentinel value: "use the configured default model from CoreConfig/ModelPreference".
    ///
    /// ## 使用场景
    /// - CLI `--model` 参数的默认值
    /// - API 请求未指定模型时的占位符
    /// - 任何"我不关心具体模型，让配置决定"的调用点
    ///
    /// ## 不应出现的场景
    /// - ProviderRegistry::resolve() 的输入（必须先 resolve 为具体模型）
    /// - ModelCatalog 的 key（catalog 只存具体模型）
    pub const AUTO: &'static str = "auto";

    /// 检查此 ModelId 是否为 AUTO sentinel（需要进一步解析为具体模型）
    pub fn is_auto(&self) -> bool {
        self.0 == Self::AUTO || self.0.is_empty()
    }

    /// 创建 AUTO sentinel 实例
    pub fn auto() -> Self {
        Self(Self::AUTO.to_string())
    }
}

impl From<&str> for ModelId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Opaque provider identifier (e.g. "anthropic", "deepseek", "openai")
#[derive(Debug, Clone, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderId(pub String);

impl From<&str> for ProviderId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ─── QualifiedModelId: provider-qualified model reference ───────────────────

/// A model identifier optionally qualified with a provider prefix.
///
/// Format: `"provider:model"` (qualified) or `"model"` (unqualified).
///
/// ## Examples
/// - `"anthropic:claude-opus-4-7"` — fully qualified
/// - `"deepseek-v4-flash"` — unqualified (provider resolved by context)
///
/// ## Edge cases handled by `parse()`:
/// - Empty string → unqualified with empty model id
/// - Multiple colons (e.g. `"host:port:model"`) → first segment is provider, rest is model
/// - Whitespace → trimmed from both provider and model
///
/// ## References
/// - Created by: CLI `/model` command, config file deserialization, `ModelPreference`
/// - Consumed by: model router (provider selection), display layer
///
/// ## Lifecycle
/// - Typically per-session or per-config-load; immutable after construction
#[derive(Debug, Clone, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QualifiedModelId {
    /// Provider portion; `None` means unqualified (provider inferred by context).
    pub provider: Option<ProviderId>,
    /// The model identifier itself.
    pub model: ModelId,
}

impl QualifiedModelId {
    /// Parse a string into a `QualifiedModelId`.
    ///
    /// Rules:
    /// - If `input` contains `':'`, the first segment becomes the provider, the rest becomes the model.
    /// - Whitespace is trimmed from both parts.
    /// - An empty provider segment (e.g. `":model"`) results in `provider = None`.
    /// - An input with no `':'` is treated as unqualified (model only).
    pub fn parse(input: &str) -> Self {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Self {
                provider: None,
                model: ModelId(String::new()),
            };
        }

        match trimmed.find(':') {
            Some(pos) => {
                let provider_str = trimmed[..pos].trim();
                let model_str = trimmed[pos + 1..].trim();
                let provider = if provider_str.is_empty() {
                    None
                } else {
                    Some(ProviderId(provider_str.to_string()))
                };
                Self {
                    provider,
                    model: ModelId(model_str.to_string()),
                }
            }
            None => Self {
                provider: None,
                model: ModelId(trimmed.to_string()),
            },
        }
    }

    /// Returns `true` if a provider is specified.
    pub fn is_qualified(&self) -> bool {
        self.provider.is_some()
    }

    /// Returns the model name as a string slice.
    pub fn model_name(&self) -> &str {
        &self.model.0
    }

    /// Returns the provider name if qualified, or `None`.
    pub fn provider_name(&self) -> Option<&str> {
        self.provider.as_ref().map(|p| p.0.as_str())
    }
}

impl std::fmt::Display for QualifiedModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.provider {
            Some(p) => write!(f, "{}:{}", p.0, self.model.0),
            None => write!(f, "{}", self.model.0),
        }
    }
}

impl From<&str> for QualifiedModelId {
    fn from(s: &str) -> Self {
        Self::parse(s)
    }
}

impl From<ModelId> for QualifiedModelId {
    fn from(model: ModelId) -> Self {
        Self { provider: None, model }
    }
}

impl From<(ProviderId, ModelId)> for QualifiedModelId {
    fn from((provider, model): (ProviderId, ModelId)) -> Self {
        Self { provider: Some(provider), model }
    }
}

/// Capability flags a model advertises
#[derive(Debug, Clone)]
pub struct CapabilitySet {
    pub streaming: bool,
    pub tools: bool,
    pub parallel_tool_calls: bool,
    pub vision: bool,
    pub thinking: bool,
    pub json_mode: bool,
    pub image_generation: bool,
    pub embedding: bool,
    pub rerank: bool,
    pub batch: bool,
    pub fine_tuning: bool,
}

impl Default for CapabilitySet {
    fn default() -> Self {
        Self {
            streaming: true,
            tools: true,
            parallel_tool_calls: true,
            vision: false,
            thinking: false,
            json_mode: true,
            image_generation: false,
            embedding: false,
            rerank: false,
            batch: false,
            fine_tuning: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum SchemaFormat {
    JsonSchema,
    JsonObject,
    Text,
}

#[derive(Debug, Clone)]
pub struct RateLimits {
    pub requests_per_minute: u32,
    pub tokens_per_minute: u32,
    pub max_concurrent: u32,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            requests_per_minute: 60,
            tokens_per_minute: 1_000_000,
            max_concurrent: 8,
        }
    }
}

#[derive(Debug, Clone)]
pub enum LatencyTier {
    RealTime,
    Interactive,
    Batch,
}

/// Thinking depth levels — legacy抽象, kept for backward compatibility.
///
/// **注意**：自 Phase 1 起新代码应优先使用 [`ThinkingIntent`]/[`EffortLevel`]，
/// 这两个新类型与各厂商官方 API（Anthropic adaptive/extended、DeepSeek 双协议、
/// OpenAI minimal、Gemini thinkingBudget）形成精确映射；`ThinkingEffort` 是
/// 有损中间表示，仅保留为已部署调用点的桥梁，会在 Phase 5 标记 `#[deprecated]`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum ThinkingEffort {
    /// No thinking (disabled)
    Off,
    /// Light reasoning — fast, lower token cost
    /// DeepSeek: reasoning_effort="low", budget ~2048
    Low,
    /// Balanced reasoning
    /// DeepSeek: reasoning_effort="medium", budget ~8192
    Medium,
    /// Deep reasoning — no artificial limit
    /// DeepSeek: reasoning_effort="high" (default, no budget cap)
    #[default]
    High,
}

impl ThinkingEffort {
    /// Convert to DeepSeek reasoning_effort string
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Parse from user input (CLI / REPL)
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "off" | "none" | "0" | "disabled" => Some(Self::Off),
            "low" | "1" | "light" => Some(Self::Low),
            "medium" | "med" | "2" | "moderate" => Some(Self::Medium),
            "high" | "deep" | "3" | "max" => Some(Self::High),
            _ => None,
        }
    }

    /// Whether thinking is actually enabled at this level
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Off)
    }
}

// ─── Phase 1: 三层抽象的 L1 + L2 类型 ──────────────────────────────────────

/// L1：用户意图层。表达「用户想让模型思考多少」，与 provider 无关。
///
/// ## 引用关系
/// - 创建：CLI 参数解析、ConfigManager.get_thinking_intent()、TUI /thinking 命令、
///   Specialist YAML、HTTP API 请求体
/// - 消费：每个 provider 内部的 `resolve_thinking()` 把 Intent 翻译成 native 配置
///
/// ## 生命周期
/// - per-process: ConfigManager 默认值（启动时构建，不变）
/// - per-request: RequestContext.thinking_intent（每轮 turn 重建）
/// - per-message: 不需要——一个 turn 内 intent 一致
///
/// ## 与旧 `ThinkingEffort` 的关系
/// `ThinkingEffort::Off` → `ThinkingIntent::Off`；其余通过 `From<ThinkingEffort>` 升格到
/// `ThinkingIntent::Effort(EffortLevel::*)`。反向映射有损（Adaptive/Budget/Max/XHigh/Minimal
/// 不能精确还原），故只提供单向 From。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ThinkingIntent {
    /// 不思考。serde: "off"
    #[default]
    Off,
    /// 让模型自主决定。Anthropic Opus 4.7+ 唯一模式；Gemini -1。serde: "adaptive"
    Adaptive,
    /// 档位提示。最常见路径。serde: "low"|"medium"|"high"|"max"|"xhigh"|"minimal"
    Effort(EffortLevel),
    /// 显式 token 预算（高级/Gemini-friendly）。serde: 整数（如 8192）
    Budget(u32),
}

impl ThinkingIntent {
    /// 是否实际启用思考
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// 从用户输入字符串宽松解析。整数会解析为 `Budget(n)`。
    /// 接受：off/none/0/disabled、adaptive/auto、minimal/min、low、medium、
    /// high、max、xhigh/x-high、整数。
    pub fn from_str_loose(s: &str) -> Option<Self> {
        let s = s.trim().to_lowercase();
        // 整数 → Budget
        if let Ok(n) = s.parse::<u32>() {
            return Some(if n == 0 { Self::Off } else { Self::Budget(n) });
        }
        match s.as_str() {
            "off" | "none" | "disabled" => Some(Self::Off),
            "adaptive" | "auto" | "dynamic" => Some(Self::Adaptive),
            "minimal" | "min" => Some(Self::Effort(EffortLevel::Minimal)),
            "low" | "light" => Some(Self::Effort(EffortLevel::Low)),
            "medium" | "med" | "moderate" => Some(Self::Effort(EffortLevel::Medium)),
            "high" | "deep" => Some(Self::Effort(EffortLevel::High)),
            "max" | "maximum" => Some(Self::Effort(EffortLevel::Max)),
            "xhigh" | "x-high" | "extra-high" => Some(Self::Effort(EffortLevel::XHigh)),
            _ => None,
        }
    }

    /// 序列化为字符串（用于配置回写、日志）
    pub fn to_str(&self) -> String {
        match self {
            Self::Off => "off".into(),
            Self::Adaptive => "adaptive".into(),
            Self::Effort(e) => e.as_str().into(),
            Self::Budget(n) => n.to_string(),
        }
    }
}

impl From<ThinkingEffort> for ThinkingIntent {
    /// 旧 → 新：单向有损 lift（保证不丢已用语义）。
    /// `Off` → `Off`；其余 → `Effort(...)`。
    fn from(e: ThinkingEffort) -> Self {
        match e {
            ThinkingEffort::Off => Self::Off,
            ThinkingEffort::Low => Self::Effort(EffortLevel::Low),
            ThinkingEffort::Medium => Self::Effort(EffortLevel::Medium),
            ThinkingEffort::High => Self::Effort(EffortLevel::High),
        }
    }
}

/// L1：档位枚举（不含 Off——Off 归入 [`ThinkingIntent::Off`]）。
///
/// ## 各厂商映射（详见 thinking_resolver）
/// - **OpenAI**：Minimal→"minimal" (GPT-5)、Low/Medium/High → 同名字符串
/// - **Anthropic adaptive**：Low/Medium/High/Max/XHigh → "low"/"medium"/"high"/"max"/"xhigh"
/// - **Anthropic extended**：经 budget_tokens(level) 映射成整数 (1024~64000)
/// - **DeepSeek OpenAI 格式**：所有档位都退化为 enabled toggle（API 不支持档位字段）
/// - **DeepSeek Anthropic 格式**：Low/Medium/High → "high"（API 强制升档）；Max/XHigh → "max"
/// - **Gemini**：经 budget_tokens(level) 映射成 thinkingBudget 整数
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffortLevel {
    /// 极简推理（OpenAI GPT-5 专属档位）
    Minimal,
    /// 轻量推理
    Low,
    /// 平衡（多家厂商默认）
    Medium,
    /// 深度推理
    High,
    /// 最大档位（Anthropic Opus 4.6+/DeepSeek Anthropic 格式）
    Max,
    /// 极高档位（Anthropic Opus 4.7 编码场景）
    XHigh,
}

impl EffortLevel {
    /// 序列化为字符串（与多数厂商 API 字段值对齐）
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
            Self::XHigh => "xhigh",
        }
    }

    /// 默认 token 预算（用于 Anthropic extended_thinking / Gemini thinkingBudget）。
    /// 数值参考各厂商文档典型值，不是硬性约束。
    pub fn default_budget_tokens(&self) -> u32 {
        match self {
            Self::Minimal => 512,
            Self::Low => 2048,
            Self::Medium => 8192,
            Self::High => 16384,
            Self::Max => 32768,
            Self::XHigh => 64000,
        }
    }

    /// 排序辅助：用于"模型不支持此档位时降级到最接近的支持档"
    pub fn rank(&self) -> u8 {
        match self {
            Self::Minimal => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::Max => 4,
            Self::XHigh => 5,
        }
    }
}

/// L2：模型支持的"思考模式种类"。一个模型可能支持多种（如 Anthropic Opus 4.6 同时
/// 支持 ExtendedBudget 和 AdaptiveEffort），决定 resolver 走哪条分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThinkingModeKind {
    /// 仅 enabled/disabled 开关，不接受档位/预算（DeepSeek OpenAI 格式）
    EnabledToggle,
    /// 整数 token 预算（Anthropic extended_thinking、Gemini thinkingBudget）
    ExtendedBudget,
    /// effort 字符串档位（Anthropic adaptive、OpenAI reasoning_effort、DeepSeek Anthropic 格式）
    AdaptiveEffort,
    /// budget int 但额外支持负数特殊语义（Gemini -1=动态/0=禁用）
    BudgetInt,
}

/// L2：多轮 reasoning content 回传协议。各厂商不一致：
/// - **None**：响应中的思考块不可回传给 API（Anthropic：thinking block 是只读响应字段）
/// - **ReasoningContent**：必须以 `reasoning_content` 字段原样回传（DeepSeek thinking mode）
/// - **Signature**：以签名/handle 引用先前思考（部分模型的延伸协议，预留）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiTurnReplay {
    None,
    ReasoningContent,
    Signature,
}

/// L2：单一模型的"思考能力声明"。Resolver 用它决定如何把 [`ThinkingIntent`] 翻译
/// 到具体 provider native 参数；catalog 维护每个已知模型的实例。
///
/// ## 引用关系
/// - 创建：`abacus-core::llm::model_catalog::ModelCatalog::builtin()`（内置）+
///   `merge_toml(~/.abacus/models.toml)`（用户覆盖）
/// - 消费：各 provider 的 `resolve_thinking(&ThinkingIntent, &ModelSpec)`
///
/// ## 生命周期
/// - 创建：CoreLoop 启动时一次
/// - 销毁：进程退出
///
/// ## 默认值
/// `ThinkingCapabilities::none()`：模型完全不支持 thinking（如纯文本 GPT-3.5）
#[derive(Debug, Clone)]
pub struct ThinkingCapabilities {
    /// 该模型支持哪几种 mode kind。空 vec 表示不支持思考。
    pub supported_modes: Vec<ThinkingModeKind>,
    /// 当 supported_modes 多于一种时，preferred 哪一种（用于 Adaptive intent 路由）
    pub default_mode: Option<ThinkingModeKind>,
    /// 该模型实际接受的档位字符串（非空且 supported_modes 含 AdaptiveEffort 时生效）
    pub effort_levels: Vec<EffortLevel>,
    /// 整数预算的合法区间（含两端），(min, max)。仅 ExtendedBudget/BudgetInt 有意义。
    pub budget_range: Option<(u32, u32)>,
    /// 多轮回传协议
    pub multi_turn_replay: MultiTurnReplay,
}

impl ThinkingCapabilities {
    /// 不支持思考的模型用此实例。serde: 等价于「字段缺省」
    pub fn none() -> Self {
        Self {
            supported_modes: Vec::new(),
            default_mode: None,
            effort_levels: Vec::new(),
            budget_range: None,
            multi_turn_replay: MultiTurnReplay::None,
        }
    }

    /// 是否支持思考（任何 mode）
    pub fn is_supported(&self) -> bool {
        !self.supported_modes.is_empty()
    }

    /// 是否支持 adaptive intent 直接路由（即 supported_modes 含 AdaptiveEffort）
    pub fn supports_adaptive(&self) -> bool {
        self.supported_modes.contains(&ThinkingModeKind::AdaptiveEffort)
    }

    /// 是否支持 budget int（ExtendedBudget 或 BudgetInt）
    pub fn supports_budget(&self) -> bool {
        self.supported_modes.iter().any(|m| {
            matches!(m, ThinkingModeKind::ExtendedBudget | ThinkingModeKind::BudgetInt)
        })
    }
}

impl Default for ThinkingCapabilities {
    fn default() -> Self { Self::none() }
}

/// Per-model thinking configuration.
///
/// **注意**：Phase 1 起此类型仅作历史兼容入口（engine_init / server 旧路径），
/// 新代码应通过 `ModelSpec.thinking_capabilities` 表达模型能力，配合
/// [`ThinkingIntent`] 表达用户意图。
#[derive(Debug, Clone)]
#[derive(Default)]
pub struct ModelThinkingConfig {
    /// Whether thinking is enabled for this model.
    pub enabled: bool,
    /// Reasoning effort level (provider-specific interpretation).
    pub effort: Option<ThinkingEffort>,
    /// Preserve thinking content across turns (Preserved Thinking).
    pub preserve_thinking: bool,
}


/// Per-model specification (hardware-level constraints + model capabilities)
#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub max_temperature: f64,
    pub min_temperature: f64,
    pub max_top_p: f64,
    pub max_n: u32,
    pub max_stop_sequences: usize,
    pub max_tools: usize,
    pub supported_schemas: Vec<SchemaFormat>,
    pub rate_limits: RateLimits,
    pub latency_tier: LatencyTier,
    /// Maximum context window in tokens (e.g. 128_000, 1_000_000)
    pub context_window: usize,
    /// Maximum output tokens (e.g. 4096, 8192, 384_000)
    pub max_output_tokens: u32,
    /// Thinking configuration for this model（旧路径——CoreConfig.model_spec 仍读此字段）
    pub thinking_config: ModelThinkingConfig,
    /// 新增（Phase 1）：精确的思考能力声明，由 ModelCatalog 维护。
    /// Resolver 优先读此字段；若 capability 缺省（none()）则 fall back 到 thinking_config。
    pub thinking_capabilities: ThinkingCapabilities,
}

impl Default for ModelSpec {
    fn default() -> Self {
        Self {
            max_temperature: 1.0,
            min_temperature: 0.0,
            max_top_p: 1.0,
            max_n: 1,
            max_stop_sequences: 4,
            max_tools: 64,
            supported_schemas: vec![SchemaFormat::Text],
            rate_limits: RateLimits::default(),
            latency_tier: LatencyTier::Interactive,
            context_window: 1_000_000,  // DeepSeek-V4 全系原生 1M context
            max_output_tokens: 8192,
            thinking_config: ModelThinkingConfig::default(),
            thinking_capabilities: ThinkingCapabilities::none(),
        }
    }
}

/// Per-model pricing
///
/// 字段单位：**USD per token**（不是 per 1M tokens）
///
/// 例：DeepSeek-V4 Pro 价格 $1.74 / 1M tokens，写作 `per_input_token: 1.74e-6`
///
/// 引用关系：
/// - 生产者：lookup_pricing(model) 静态返回；abacus-core/llm/providers 也用同一表
/// - 消费者：Pricing::input_cost / output_cost；abacus-cli/tui/cost.rs 通过 lookup_pricing 间接使用
///
/// 历史 bug：V28.7（2026-05）修复——曾把字段语义注释为 "USD per 1M tokens"，但
/// 公式又 `tokens * per_input_token / 1_000_000`（双重单位换算导致结果偏小百万倍）。
/// 因 input_cost/output_cost 当时无 caller 是 dormant bug，cli 端绕开本 API 自实现。
/// 修复后字段语义统一为 "USD per token"，公式改 `tokens * per_input_token`。
#[derive(Debug, Clone, Copy)]
pub struct Pricing {
    /// USD per input token（如 1.74e-6 = $1.74/M）
    pub per_input_token: f64,
    /// USD per output token
    pub per_output_token: f64,
    /// USD per cached input token；None → 走 per_input_token 不打折
    pub per_cached_input: Option<f64>,
    /// 每次请求固定费（少数 provider 收）
    pub per_request: Option<f64>,
}

impl Pricing {
    /// 输入侧费用：未命中缓存部分按 per_input_token，命中缓存部分按 per_cached_input
    pub fn input_cost(&self, tokens: u64, cached: u64) -> f64 {
        let uncached = tokens.saturating_sub(cached);
        let cached_rate = self.per_cached_input.unwrap_or(self.per_input_token);
        uncached as f64 * self.per_input_token + cached as f64 * cached_rate
    }

    /// 输出侧费用
    pub fn output_cost(&self, tokens: u64) -> f64 {
        tokens as f64 * self.per_output_token
    }

    /// 单轮总费用 = input + output (+ per_request if any)
    pub fn turn_cost(&self, prompt: u64, completion: u64, cached: u64) -> f64 {
        self.input_cost(prompt, cached) + self.output_cost(completion) + self.per_request.unwrap_or(0.0)
    }
}

/// V31: 按 model_id 查 Pricing —— 委托给 model_registry::lookup_model_or_default
///
/// ## 引用关系
/// - 调用方：abacus-core/llm/providers/deepseek.rs（构造 provider 时绑定）；
///   abacus-cli/tui/cost.rs（每轮统计费用）
/// - 数据来源：model_registry::MODELS（按 model_id 精确匹配）
/// - fallback：未知模型走 V4-Flash 价位（保守估算）
///
/// ## V31 改动
/// - 旧版：substring 匹配 `contains("pro")`，脆弱（gemini-2.5-pro 误命中 DeepSeek-Pro 价位）
/// - 新版：精确 model_id 索引；价格 source-of-truth 改为 CNY，本函数现算 USD（用 DEFAULT_CNY_TO_USD_RATE=7.10）
/// - V4-Pro 价格修正：从历史 $1.74/$3.48/$0.0145 → 实际 ¥3/¥6/¥0.025（折 USD ≈ $0.42/$0.85/$0.0035）
///
/// ## 兼容性
/// 保留本函数签名不变（返 Pricing 字段 USD per token），现存所有调用点透明升级
pub fn lookup_pricing(model: &str) -> Pricing {
    let info = crate::model_registry::lookup_model_or_default(model);
    let fx = crate::model_registry::DEFAULT_CNY_TO_USD_RATE;
    Pricing {
        per_input_token: info.price_input_cny / fx,
        per_output_token: info.price_output_cny / fx,
        per_cached_input: Some(info.price_cached_input_cny / fx),
        per_request: info.price_per_request_cny.map(|c| c / fx),
    }
}

#[cfg(test)]
mod pricing_tests {
    use super::*;

    /// V31: V4-Pro 1M+1M 实际计费 ¥9，按 7.10 汇率 ≈ $1.268
    /// （旧测试期望 $5.22 是 4/26 降价前的原价，已偏离 4 倍）
    #[test]
    fn pro_1m_each_costs_post_2026_04_26_pricing() {
        let p = lookup_pricing("deepseek-v4-pro");
        let cost = p.turn_cost(1_000_000, 1_000_000, 0);
        // ¥(3+6) / 7.10 ≈ $1.268
        assert!((cost - 1.268).abs() < 0.01, "got {}", cost);
    }

    /// V31: V4-Pro 1M cached 实际 ¥0.025，按 7.10 汇率 ≈ $0.00352
    #[test]
    fn cached_discount_v4_pro() {
        let p = lookup_pricing("deepseek-v4-pro");
        let cost = p.turn_cost(1_000_000, 0, 1_000_000);
        // ¥0.025 / 7.10 ≈ $0.00352
        assert!((cost - 0.00352).abs() < 1e-4, "got {}", cost);
    }

    /// V31: 未知模型 fallback 到 V4-Flash 价位
    /// V4-Flash 1M+1M = ¥3 / 7.10 ≈ $0.423
    #[test]
    fn unknown_falls_back_to_v4_flash() {
        let p = lookup_pricing("gpt-99-unknown");
        let cost = p.turn_cost(1_000_000, 1_000_000, 0);
        assert!((cost - 0.423).abs() < 0.01, "got {}", cost);
    }

    /// V31: legacy alias deepseek-chat 路由到 V4-Flash 价位（4/26 起官方路由）
    #[test]
    fn legacy_chat_alias_routes_to_v4_flash() {
        let p = lookup_pricing("deepseek-chat");
        let cost = p.turn_cost(1_000_000, 1_000_000, 0);
        // 与 V4-Flash 同价 ≈ $0.423
        assert!((cost - 0.423).abs() < 0.01, "got {}", cost);
    }

    /// V31: legacy alias deepseek-reasoner 也路由 V4-Flash 价位（思考模式）
    #[test]
    fn legacy_reasoner_alias_routes_to_v4_flash() {
        let p = lookup_pricing("deepseek-reasoner");
        let cost = p.turn_cost(1_000_000, 1_000_000, 0);
        assert!((cost - 0.423).abs() < 0.01, "got {}", cost);
    }
}

/// Inline model specification for configuration-driven setups
#[derive(Debug, Clone)]
pub struct InlineModelSpec {
    pub provider: ProviderId,
    pub model: ModelId,
    pub capabilities: Vec<String>,
    /// Raw specs map for provider-specific overrides
    pub specs: Option<std::collections::HashMap<String, serde_json::Value>>,
    /// Context window in tokens (overrides capability defaults)
    pub context_window: Option<usize>,
    /// Thinking depth configuration
    pub thinking: Option<ModelThinkingConfig>,
}