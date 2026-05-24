//! 模型注册表 — 按 model_id 维度的单一真相源（SSoT）
//!
//! 设计意图：
//! - 旧 lookup_pricing 用 substring 匹配（如 `contains("pro")`）脆弱且易误命中
//!   （如 `gemini-2.5-pro` 会被当作 DeepSeek-Pro 价位计费）
//! - 新设计：按精确 model_id 索引一个 const 数组；每条记录聚合定价、能力、限制、生命周期
//! - 价格**以 CNY 为 canonical**（DeepSeek 官方计费货币），USD 经汇率现算
//!
//! 引用关系：
//! - 写：仅本文件 `MODELS` const（加新模型只改这一处）
//! - 读：`abacus-types::lookup_pricing`（兼容老 API）、`abacus-core/llm/providers/deepseek.rs`
//!       （prefix/beta 能力查询）、`abacus-cli/tui/cost.rs`（CNY 计费）、
//!       `abacus-cli/tui/components/mod.rs`（看板显示）、`abacus-cli/engine_init.rs`
//!       （context_window fallback）
//!
//! 生命周期：
//! - 编译期 const，零运行时开销
//! - 价格同步：DeepSeek 调价时（如 4/26 V4 全系降价）人工更新本表
//!
//! 校准依据（最近一次同步：2026-05-24）：
//! - DeepSeek 官方 ¥ pricing：https://api-docs.deepseek.com/zh-cn/quick_start/pricing
//! - V4 release notes：https://api-docs.deepseek.com/news/news260424
//! - 4/26 全系降价公告 + V4-Pro 2.5 折永久生效（5/31 23:59 后正式调整为原价 1/4）

/// 单条模型元信息 — 定价 + 能力 + 限制 + 生命周期
///
/// ## 引用关系
/// - 由 `lookup_model(id)` / `lookup_model_or_default(id)` 返回静态引用
/// - 价格字段单位：**CNY per token**（不是 per 1M tokens）
///   例：DeepSeek-V4-Pro ¥3/M tokens → `price_input_cny: 3.0e-6`
///
/// ## 生命周期
/// - 'static const，整个进程生命周期不变
#[derive(Debug, Clone, Copy)]
pub struct ModelInfo {
    /// 精确 model_id（API 调用用的字符串）
    pub id: &'static str,
    /// 人类友好显示名
    pub display_name: &'static str,
    /// provider 标识（"deepseek"/"anthropic"/...）
    pub provider: &'static str,

    // ── Pricing (CNY canonical, per token) ──
    /// 输入 token 单价（CNY，未命中缓存）
    pub price_input_cny: f64,
    /// 输出 token 单价（CNY）
    pub price_output_cny: f64,
    /// 缓存命中输入 token 单价（CNY）
    pub price_cached_input_cny: f64,
    /// 每次请求固定费（少数 provider 有），CNY；None=无
    pub price_per_request_cny: Option<f64>,

    // ── Capabilities ──
    /// 是否支持 prefix completion（DeepSeek /beta API）
    pub supports_prefix_completion: bool,
    /// 是否支持思考模式（V4 全系 / o1 / claude extended thinking 等）
    pub supports_thinking_mode: bool,
    /// 是否支持 FIM（Fill-In-the-Middle）补全
    pub supports_fim_completion: bool,
    /// 是否支持 JSON output 强约束
    pub supports_json_output: bool,
    /// 是否支持 function/tool calling
    pub supports_tool_calling: bool,

    // ── Limits ──
    /// 上下文窗口（tokens）
    pub context_window: u32,
    /// 单次响应最大输出 tokens
    pub max_output_tokens: u32,
    /// 思考模式 CoT 最大 tokens；None=不支持思考模式或无限制
    pub max_cot_tokens: Option<u32>,

    // ── Lifecycle ──
    /// alias 路由目标（legacy alias 指向新模型）；None=非 alias
    pub aliased_to: Option<&'static str>,
    /// 弃用日期（YYYY-MM-DD）；None=无弃用计划
    pub deprecation_date: Option<&'static str>,
    /// alias 路由时是否对应思考模式（reasoner→true, chat→false）
    pub thinking_mode_alias: bool,
}

impl ModelInfo {
    /// 单轮总费用（CNY）= input + output + 缓存折扣 + per-request
    pub fn turn_cost_cny(&self, prompt: u64, completion: u64, cached: u64) -> f64 {
        let uncached = prompt.saturating_sub(cached);
        uncached as f64 * self.price_input_cny
            + cached as f64 * self.price_cached_input_cny
            + completion as f64 * self.price_output_cny
            + self.price_per_request_cny.unwrap_or(0.0)
    }

    /// 单轮总费用（USD）= turn_cost_cny ÷ fx_rate
    /// fx_rate 单位：USD/CNY，默认 7.10（接近当前实情）
    pub fn turn_cost_usd(&self, prompt: u64, completion: u64, cached: u64, fx_rate: f64) -> f64 {
        if fx_rate <= 0.0 {
            return 0.0;
        }
        self.turn_cost_cny(prompt, completion, cached) / fx_rate
    }

    /// 输入侧费用（CNY）独立调用
    pub fn input_cost_cny(&self, tokens: u64, cached: u64) -> f64 {
        let uncached = tokens.saturating_sub(cached);
        uncached as f64 * self.price_input_cny + cached as f64 * self.price_cached_input_cny
    }

    /// 输出侧费用（CNY）独立调用
    pub fn output_cost_cny(&self, tokens: u64) -> f64 {
        tokens as f64 * self.price_output_cny
    }
}

/// DeepSeek + 兜底模型注册表（按 model_id 索引）
///
/// 顺序约定：fallback 用第一条（V4-Flash），所以 V4-Flash 必须放第一位
const MODELS: &[ModelInfo] = &[
    // ─────────────────────────────────────────────────────────
    // DeepSeek V4 系列（当前主力，2026-04-24 发布，4/26 起降价生效）
    // ─────────────────────────────────────────────────────────

    // V4-Flash 放第一位作为 fallback
    ModelInfo {
        id: "deepseek-v4-flash",
        display_name: "DeepSeek V4 Flash",
        provider: "deepseek",
        // 4/26 公告：input miss ¥1, output ¥2, cache hit ¥0.02 (降至 1/10)
        price_input_cny: 1.0e-6,         // ¥1/M
        price_output_cny: 2.0e-6,        // ¥2/M
        price_cached_input_cny: 0.02e-6, // ¥0.02/M
        price_per_request_cny: None,
        supports_prefix_completion: true,
        supports_thinking_mode: true,
        supports_fim_completion: true, // 仅非思考模式
        supports_json_output: true,
        supports_tool_calling: true,
        context_window: 1_000_000,
        max_output_tokens: 384_000,
        max_cot_tokens: Some(32_000),
        aliased_to: None,
        deprecation_date: None,
        thinking_mode_alias: false,
    },
    ModelInfo {
        id: "deepseek-v4-pro",
        display_name: "DeepSeek V4 Pro",
        provider: "deepseek",
        // 4/26 公告 + 2.5 折永久生效（5/31 后正式调整为原价 1/4，价格不变）
        // input miss ¥3, output ¥6, cache hit ¥0.025
        price_input_cny: 3.0e-6,          // ¥3/M
        price_output_cny: 6.0e-6,         // ¥6/M
        price_cached_input_cny: 0.025e-6, // ¥0.025/M
        price_per_request_cny: None,
        supports_prefix_completion: true,
        supports_thinking_mode: true,
        supports_fim_completion: true,
        supports_json_output: true,
        supports_tool_calling: true,
        context_window: 1_000_000,
        max_output_tokens: 384_000,
        max_cot_tokens: Some(64_000),
        aliased_to: None,
        deprecation_date: None,
        thinking_mode_alias: false,
    },
    // ─────────────────────────────────────────────────────────
    // Legacy aliases（V3.2 兼容路由，2026-07-24 弃用）
    // 自 4/26 起官方路由到 V4-Flash 思考/非思考模式 → 价格继承 V4-Flash
    // ─────────────────────────────────────────────────────────
    ModelInfo {
        id: "deepseek-chat",
        display_name: "DeepSeek Chat (alias→V4-Flash 非思考)",
        provider: "deepseek",
        // 路由到 V4-Flash 非思考模式 → 实际计费 V4-Flash 价位
        price_input_cny: 1.0e-6,
        price_output_cny: 2.0e-6,
        price_cached_input_cny: 0.02e-6,
        price_per_request_cny: None,
        supports_prefix_completion: false, // legacy alias 不走 beta
        supports_thinking_mode: false,
        supports_fim_completion: true,
        supports_json_output: true,
        supports_tool_calling: true,
        context_window: 64_000, // 老 alias 限制
        max_output_tokens: 8_000,
        max_cot_tokens: None,
        aliased_to: Some("deepseek-v4-flash"),
        deprecation_date: Some("2026-07-24"),
        thinking_mode_alias: false,
    },
    ModelInfo {
        id: "deepseek-reasoner",
        display_name: "DeepSeek Reasoner (alias→V4-Flash 思考)",
        provider: "deepseek",
        // 路由到 V4-Flash 思考模式
        price_input_cny: 1.0e-6,
        price_output_cny: 2.0e-6,
        price_cached_input_cny: 0.02e-6,
        price_per_request_cny: None,
        supports_prefix_completion: false,
        supports_thinking_mode: true, // alias 路由思考模式
        supports_fim_completion: false, // 思考模式不支持 FIM
        supports_json_output: true,
        supports_tool_calling: true,
        context_window: 64_000,
        max_output_tokens: 8_000,
        max_cot_tokens: Some(32_000),
        aliased_to: Some("deepseek-v4-flash"),
        deprecation_date: Some("2026-07-24"),
        thinking_mode_alias: true,
    },
];

/// 按精确 id 查询模型；找不到返回 None
pub fn lookup_model(id: &str) -> Option<&'static ModelInfo> {
    let lower = id.to_ascii_lowercase();
    MODELS.iter().find(|m| m.id == lower)
}

/// 按精确 id 查询模型；找不到 fallback 到 V4-Flash 价位（保守估算，不误导）
///
/// 设计选择：fallback 用最便宜的主力模型，避免：
/// - 返 0：让用户误以为"免费"
/// - 抛错：让 cli 路径崩溃，影响显示
/// - 用 Pro 价：未知模型按高价显示，吓人不准确
pub fn lookup_model_or_default(id: &str) -> &'static ModelInfo {
    lookup_model(id).unwrap_or(&MODELS[0]) // V4-Flash 永远在第一位
}

/// 取所有已注册模型（用于 UI picker / 配置 init wizard）
pub fn all_models() -> &'static [ModelInfo] {
    MODELS
}

/// 取指定 provider 的所有模型
pub fn models_by_provider<'a>(provider: &'a str) -> impl Iterator<Item = &'static ModelInfo> + 'a {
    MODELS.iter().filter(move |m| m.provider == provider)
}

/// 默认 USD/CNY 汇率（更接近 2026-05 实情，原 7.2 偏高）
pub const DEFAULT_CNY_TO_USD_RATE: f64 = 7.10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_v4_pro_exact() {
        let m = lookup_model("deepseek-v4-pro").unwrap();
        assert_eq!(m.id, "deepseek-v4-pro");
        assert_eq!(m.price_input_cny, 3.0e-6);
        assert!(m.supports_prefix_completion);
        assert!(m.supports_thinking_mode);
    }

    #[test]
    fn lookup_v4_flash_exact() {
        let m = lookup_model("deepseek-v4-flash").unwrap();
        assert_eq!(m.price_input_cny, 1.0e-6);
        assert_eq!(m.price_cached_input_cny, 0.02e-6);
    }

    #[test]
    fn lookup_legacy_chat_routes_to_flash_pricing() {
        let m = lookup_model("deepseek-chat").unwrap();
        // legacy alias 价格继承 V4-Flash
        assert_eq!(m.price_input_cny, 1.0e-6);
        assert_eq!(m.price_output_cny, 2.0e-6);
        assert!(!m.supports_thinking_mode); // chat = 非思考
        assert_eq!(m.aliased_to, Some("deepseek-v4-flash"));
        assert!(m.deprecation_date.is_some());
    }

    #[test]
    fn lookup_legacy_reasoner_thinking_alias() {
        let m = lookup_model("deepseek-reasoner").unwrap();
        assert!(m.supports_thinking_mode);
        assert!(!m.supports_fim_completion); // 思考模式不支持 FIM
        assert!(m.thinking_mode_alias);
    }

    #[test]
    fn unknown_model_falls_back_to_v4_flash() {
        let m = lookup_model_or_default("gpt-99-quantum");
        assert_eq!(m.id, "deepseek-v4-flash");
    }

    #[test]
    fn case_insensitive_lookup() {
        assert!(lookup_model("DeepSeek-V4-Pro").is_some());
        assert!(lookup_model("DEEPSEEK-V4-FLASH").is_some());
    }

    #[test]
    fn turn_cost_cny_v4_pro_1m_each() {
        // V4-Pro 1M input miss + 1M output, 0 cached
        // = 1M × 3e-6 + 1M × 6e-6 = 3 + 6 = ¥9
        let m = lookup_model("deepseek-v4-pro").unwrap();
        let cost = m.turn_cost_cny(1_000_000, 1_000_000, 0);
        assert!((cost - 9.0).abs() < 1e-6, "got {}", cost);
    }

    #[test]
    fn turn_cost_cny_v4_flash_with_cache() {
        // V4-Flash 1M cached input + 1M output
        // = 1M × 0.02e-6 + 1M × 2e-6 = 0.02 + 2 = ¥2.02
        let m = lookup_model("deepseek-v4-flash").unwrap();
        let cost = m.turn_cost_cny(1_000_000, 1_000_000, 1_000_000);
        assert!((cost - 2.02).abs() < 1e-6, "got {}", cost);
    }

    #[test]
    fn turn_cost_usd_uses_fx_rate() {
        let m = lookup_model("deepseek-v4-pro").unwrap();
        let cny = m.turn_cost_cny(1_000_000, 1_000_000, 0);
        let usd = m.turn_cost_usd(1_000_000, 1_000_000, 0, 7.10);
        assert!((usd - cny / 7.10).abs() < 1e-6);
    }

    #[test]
    fn fx_rate_zero_returns_zero_safely() {
        let m = lookup_model("deepseek-v4-pro").unwrap();
        assert_eq!(m.turn_cost_usd(1000, 1000, 0, 0.0), 0.0);
    }

    #[test]
    fn all_models_v4_flash_first() {
        // fallback 依赖 MODELS[0] = V4-Flash 不变
        assert_eq!(MODELS[0].id, "deepseek-v4-flash");
    }

    #[test]
    fn deepseek_provider_filter() {
        let count = models_by_provider("deepseek").count();
        assert!(count >= 4, "should have at least 4 deepseek models, got {}", count);
    }

    #[test]
    fn v4_pro_is_4x_cheaper_than_old_code_assumed() {
        // 历史 bug 防御测试：V4-Pro input miss 应是 ¥3/M (= $0.42 @ 7.10)
        // 不是老代码的 $1.74/M（4 倍高估）
        let m = lookup_model("deepseek-v4-pro").unwrap();
        let usd_per_m = m.price_input_cny / 7.10 * 1_000_000.0;
        assert!(usd_per_m < 0.50, "V4-Pro input miss should be ~$0.42/M, got ${}", usd_per_m);
        assert!(usd_per_m > 0.40);
    }
}
