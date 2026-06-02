//! LLM provider abstraction — model catalog, thinking resolution, HTTP client
//!
//! ## Thinking Pipeline（统一决议层）
//! 之前 thinking 配置在 4 处重复定义，语义需反向解码：
//!   - config.toml: core.thinking = "off"
//!   - Anthropic: extended_thinking.budget_tokens = N
//!   - RequestContext: thinking_intent = ThinkingIntent
//!   - Specialist YAML: engagement.thinking = "high"
//!
//! ThinkingPipeline 在 RequestContext 层完成最终决议：
//!   1. 用户配置 → ThinkingIntent
//!   2. 模型能力 → clamp/fallback
//!   3. provider 只做序列化映射
//!
//! ## 拆分标记
//! 本模块内文件不拆分（~30KB 总计，合理范围）

pub mod fallback_provider;
pub mod retry_provider;
pub mod provider;
pub mod prompt_cache;
pub mod providers;
pub mod noop_provider;
pub mod stream;
pub mod model_cache;
pub mod model_catalog;
pub mod thinking_resolver;
pub mod tool_view;
pub mod text_tool_parser;
pub mod tool_catalog;
pub mod provider_registry;

pub use provider::*;
pub use prompt_cache::*;
pub use stream::*;
pub use model_cache::*;
pub use model_catalog::ModelCatalog;
pub use thinking_resolver::{
    ResolveOutcome, DegradeNote, validate_intent_against_caps,
    pick_nearest_supported, clamp_budget, highest_supported_effort,
};

// ─── 进程级共享 reqwest::Client（H8 修复）────────────────────────────────────
//
// 之前每个 LLM provider 实例独立 build 一个 Client，无法共享：
//   - 连接池：每个 provider 维护自己的池，HTTP/2 multiplex 失效
//   - DNS 缓存：每个 provider 独立解析
//   - TLS handshake：每个 provider 独立握手
//
// 共享 Client 后，所有 provider 共用底层连接池 + DNS 缓存 + 握手结果，
// 同时通过 `RequestBuilder.timeout(...)` 在每个请求上设置自己的超时。

use std::sync::OnceLock;

static SHARED_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// 进程级共享 reqwest Client（见上方 doc）
pub fn shared_http_client() -> &'static reqwest::Client {
    SHARED_HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // 连接超时：防止 DNS 解析/TCP 握手卡住（默认无限等待）
            .connect_timeout(std::time::Duration::from_secs(10))
            // 不设 Client-level timeout — 让 provider per-request timeout 生效
            // 兜底由 pipeline dynamic_timeout_secs + select! deadline 保障
            // V43.3: 连接池——缩短 idle timeout 防止死连接累积
            // 问题：长 session 中服务端可能发 GOAWAY/RST_STREAM，但客户端连接池
            // 还持有旧连接引用，下次请求直接用死连接→失败→重试也失败（池里都是死的）
            // 修复：idle 30s 即回收（原 90s），强制下次请求建新连接
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .pool_max_idle_per_host(8) // 减少池大小，加速死连接淘汰
            // TCP keepalive：及时发现断连
            .tcp_keepalive(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default()
    })
}

// ═══════════════════════════════════════════════════════════════════════════════
// ThinkingPipeline — 统一思考意图决议
// ═══════════════════════════════════════════════════════════════════════════════
//
// ## 问题
// thinking 配置在 4 个地方重复定义，语义不同：
//   - config.toml: "off" / "adaptive" / "high"
//   - Anthropic: extended_thinking.budget_tokens
//   - RequestContext: ThinkingIntent enum
//   - Specialist YAML: engagement.thinking
//
// ThinkingPipeline 在 RequestContext 层完成一次决议，provider 只做序列化。
//
// ## 流程
// 1. resolve(): 用户输入 + 模型能力 → ThinkingRequest（最终参数）
// 2. provider 调用 to_provider_specific() → 各厂商自有格式
// 3. 所有 provider 不再自己解码 thinking 语义

use abacus_types::{EffortLevel, ThinkingCapabilities, ThinkingIntent};

/// 最终思考请求参数（Provider 只需序列化此结构）
#[derive(Debug, Clone)]
pub struct ThinkingRequest {
    pub enabled: bool,
    pub budget_tokens: Option<u32>,
    pub effort: Option<EffortLevel>,
    pub adaptive: bool,
    pub display: ThinkingDisplay,
}

#[derive(Debug, Clone)]
pub enum ThinkingDisplay {
    Summarized,
    Omitted,
    Full,
}

/// 统一思考意图决议器
pub struct ThinkingPipeline;

impl ThinkingPipeline {
    /// 从用户 intent + 模型能力 → 最终 ThinkingRequest
    pub fn resolve(intent: ThinkingIntent, caps: &ThinkingCapabilities) -> ThinkingRequest {
        match intent {
            ThinkingIntent::Off => ThinkingRequest {
                enabled: false, budget_tokens: None, effort: None,
                adaptive: false, display: ThinkingDisplay::Omitted,
            },
            ThinkingIntent::Adaptive => {
                if caps.supports_adaptive() {
                    ThinkingRequest {
                        enabled: true, budget_tokens: None, effort: None,
                        adaptive: true, display: ThinkingDisplay::Summarized,
                    }
                } else {
                    // fallback: 不支持 adaptive → 用 highest supported effort
                    let best = crate::llm::thinking_resolver::highest_supported_effort(caps);
                    ThinkingRequest {
                        enabled: true, budget_tokens: None, effort: best,
                        adaptive: false, display: ThinkingDisplay::Summarized,
                    }
                }
            }
            ThinkingIntent::Effort(level) => {
                let clamped = crate::llm::thinking_resolver::pick_nearest_supported(level, &caps.effort_levels);
                ThinkingRequest {
                    enabled: true, budget_tokens: None, effort: Some(clamped),
                    adaptive: false, display: ThinkingDisplay::Summarized,
                }
            }
            ThinkingIntent::Budget(budget) => {
                let clamped = crate::llm::thinking_resolver::clamp_budget(budget, caps);
                ThinkingRequest {
                    enabled: clamped > 0, budget_tokens: Some(clamped),
                    effort: None, adaptive: false, display: ThinkingDisplay::Full,
                }
            }
        }
    }
}

#[cfg(test)]
mod thinking_tests {
    use super::*;
    use abacus_types::{EffortLevel, ThinkingCapabilities, ThinkingModeKind, MultiTurnReplay};

    fn caps_with_modes(modes: Vec<ThinkingModeKind>) -> ThinkingCapabilities {
        ThinkingCapabilities {
            supported_modes: modes,
            default_mode: None,
            effort_levels: vec![EffortLevel::Low, EffortLevel::Medium, EffortLevel::High],
            budget_range: Some((1024, 64000)),
            multi_turn_replay: MultiTurnReplay::None,
        }
    }

    #[test]
    fn test_off_disables_thinking() {
        let req = ThinkingPipeline::resolve(ThinkingIntent::Off, &ThinkingCapabilities::default());
        assert!(!req.enabled);
    }

    #[test]
    fn test_adaptive_with_capability() {
        let caps = caps_with_modes(vec![ThinkingModeKind::AdaptiveEffort]);
        let req = ThinkingPipeline::resolve(ThinkingIntent::Adaptive, &caps);
        assert!(req.adaptive);
        assert!(req.enabled);
    }

    #[test]
    fn test_adaptive_fallback_when_unsupported() {
        let caps = caps_with_modes(vec![ThinkingModeKind::ExtendedBudget]);
        let req = ThinkingPipeline::resolve(ThinkingIntent::Adaptive, &caps);
        assert!(!req.adaptive);
        assert!(req.enabled);
        assert_eq!(req.effort, Some(EffortLevel::High));
    }

    #[test]
    fn test_effort_clamps_to_supported() {
        let caps = caps_with_modes(vec![ThinkingModeKind::ExtendedBudget]);
        // XHigh not supported → clamp down to High
        let req = ThinkingPipeline::resolve(
            ThinkingIntent::Effort(EffortLevel::XHigh), &caps,
        );
        assert_eq!(req.effort, Some(EffortLevel::High));
    }
}