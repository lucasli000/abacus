//! Phase 1：用户意图 → 模型能力的验证 + 降级核心。
//!
//! 本模块**不**直接产生 provider native 请求字段（那是 Phase 2 各 provider 内部职责），
//! 仅提供：
//! 1. `validate_intent_against_caps` — 检查 intent 是否在能力范围内，输出降级建议
//! 2. `pick_supported_effort` — 在能力声明的 effort_levels 中选取最接近的档位
//! 3. `clamp_budget` — 把 budget 钳到 budget_range 内
//!
//! ## 引用关系
//! - 创建：无（pure functions + types）
//! - 消费：Phase 2 各 provider 的 `resolve_thinking()` 调用本模块预处理 intent
//!
//! ## 生命周期
//! - 不持有状态。每次 turn 都是独立调用。

use abacus_types::{
    EffortLevel, ThinkingCapabilities, ThinkingIntent, ThinkingModeKind,
};

/// Resolver 验证/降级的产出。`accepted_intent` 永远是「能力允许的最接近用户意图的版本」，
/// `notes` 列出本次降级动作（用于 warn log）。
#[derive(Debug, Clone)]
pub struct ResolveOutcome {
    pub accepted_intent: ThinkingIntent,
    pub notes: Vec<DegradeNote>,
}

/// 一次降级的记录——足以让上游打印 warn log，但不返回 Err（保持调用链不中断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DegradeNote {
    /// 模型不支持 adaptive，降级为最高 effort
    AdaptiveUnsupportedFellBackToEffort(EffortLevel),
    /// 模型不支持 adaptive 且无 effort 档位，降级为 budget
    AdaptiveUnsupportedFellBackToBudget(u32),
    /// 模型完全不支持 thinking，强制 Off
    ModelDoesNotSupportThinking,
    /// 用户请求的 effort 不在 effort_levels 内，最近档位降级
    EffortAdjusted { from: EffortLevel, to: EffortLevel },
    /// 用户请求 effort 但模型只接受 budget，按 default_budget_tokens 转换
    EffortConvertedToBudget { from: EffortLevel, budget: u32 },
    /// 用户请求 effort 但模型只接受 EnabledToggle（如 DeepSeek OpenAI 格式）——
    /// 档位丢失，转为简单 enable
    EffortDroppedForEnabledToggle(EffortLevel),
    /// budget 被钳到 [min, max]
    BudgetClamped { from: u32, to: u32 },
    /// budget 模型不支持，转换为最接近的 effort 或 enabled
    BudgetConvertedToEffort { from: u32, level: EffortLevel },
    /// budget 直接转为 EnabledToggle（DeepSeek OpenAI 格式不接受 budget）
    BudgetDroppedForEnabledToggle(u32),
}

/// 主入口：根据模型能力声明把用户 intent 规范化为「该模型实际能接受的 intent」。
///
/// 设计原则：**永不报错**。任何不兼容都降级到最接近的合法版本，让调用链继续。
/// 调用方根据返回的 `notes` 决定是否打印 warning。
pub fn validate_intent_against_caps(
    intent: &ThinkingIntent,
    caps: &ThinkingCapabilities,
) -> ResolveOutcome {
    let mut notes = Vec::new();

    // 模型完全不支持思考 → 强制 Off
    if !caps.is_supported() {
        if intent.is_enabled() {
            notes.push(DegradeNote::ModelDoesNotSupportThinking);
        }
        return ResolveOutcome { accepted_intent: ThinkingIntent::Off, notes };
    }

    let accepted = match intent {
        ThinkingIntent::Off => ThinkingIntent::Off,

        ThinkingIntent::Adaptive => {
            if caps.supports_adaptive() {
                ThinkingIntent::Adaptive
            } else if let Some(highest) = highest_supported_effort(caps) {
                notes.push(DegradeNote::AdaptiveUnsupportedFellBackToEffort(highest));
                ThinkingIntent::Effort(highest)
            } else if let Some((_, max)) = caps.budget_range {
                notes.push(DegradeNote::AdaptiveUnsupportedFellBackToBudget(max));
                ThinkingIntent::Budget(max)
            } else if caps.supported_modes.contains(&ThinkingModeKind::EnabledToggle) {
                // 仅支持 enable/disable（如 DeepSeek OpenAI 格式）——adaptive 等价于 enabled
                // 用 effort=High 代表"开启"，下游 provider 会忽略档位。
                notes.push(DegradeNote::AdaptiveUnsupportedFellBackToEffort(EffortLevel::High));
                ThinkingIntent::Effort(EffortLevel::High)
            } else {
                notes.push(DegradeNote::ModelDoesNotSupportThinking);
                ThinkingIntent::Off
            }
        }

        ThinkingIntent::Effort(requested) => {
            resolve_effort(*requested, caps, &mut notes)
        }

        ThinkingIntent::Budget(n) => {
            resolve_budget(*n, caps, &mut notes)
        }
    };

    ResolveOutcome { accepted_intent: accepted, notes }
}

/// effort 档位的降级路径
fn resolve_effort(
    requested: EffortLevel,
    caps: &ThinkingCapabilities,
    notes: &mut Vec<DegradeNote>,
) -> ThinkingIntent {
    // 模型直接接受 effort（含 AdaptiveEffort 模式）
    if caps.supports_adaptive() && !caps.effort_levels.is_empty() {
        if caps.effort_levels.contains(&requested) {
            return ThinkingIntent::Effort(requested);
        }
        // 找最近档位（用 rank 计算 |Δ| 最小）
        let nearest = pick_nearest_supported(requested, &caps.effort_levels);
        notes.push(DegradeNote::EffortAdjusted { from: requested, to: nearest });
        return ThinkingIntent::Effort(nearest);
    }

    // 模型只接受 budget int → 按 default_budget_tokens 换算 + clamp
    if caps.supports_budget() {
        let raw = requested.default_budget_tokens();
        let clamped = clamp_budget_inner(raw, caps);
        notes.push(DegradeNote::EffortConvertedToBudget { from: requested, budget: clamped });
        return ThinkingIntent::Budget(clamped);
    }

    // 模型只是 enable/disable（DeepSeek OpenAI 格式）→ 档位丢失但保留 enabled 含义
    if caps.supported_modes.contains(&ThinkingModeKind::EnabledToggle) {
        notes.push(DegradeNote::EffortDroppedForEnabledToggle(requested));
        // 用 high 代表 enabled；provider 会忽略具体档位
        return ThinkingIntent::Effort(EffortLevel::High);
    }

    notes.push(DegradeNote::ModelDoesNotSupportThinking);
    ThinkingIntent::Off
}

/// budget 的降级路径
fn resolve_budget(
    requested: u32,
    caps: &ThinkingCapabilities,
    notes: &mut Vec<DegradeNote>,
) -> ThinkingIntent {
    // 模型支持 budget → clamp 到合法区间
    if caps.supports_budget() {
        let clamped = clamp_budget_inner(requested, caps);
        if clamped != requested {
            notes.push(DegradeNote::BudgetClamped { from: requested, to: clamped });
        }
        return ThinkingIntent::Budget(clamped);
    }

    // 模型支持 effort → 找最接近请求 budget 的档位（按 default_budget_tokens 反查）
    if caps.supports_adaptive() && !caps.effort_levels.is_empty() {
        let level = nearest_effort_for_budget(requested, &caps.effort_levels);
        notes.push(DegradeNote::BudgetConvertedToEffort { from: requested, level });
        return ThinkingIntent::Effort(level);
    }

    // 模型只是 enable/disable
    if caps.supported_modes.contains(&ThinkingModeKind::EnabledToggle) {
        notes.push(DegradeNote::BudgetDroppedForEnabledToggle(requested));
        return ThinkingIntent::Effort(EffortLevel::High);
    }

    notes.push(DegradeNote::ModelDoesNotSupportThinking);
    ThinkingIntent::Off
}

/// 把 budget 钳到 caps.budget_range；若未声明范围则原样返回。
pub fn clamp_budget(budget: u32, caps: &ThinkingCapabilities) -> u32 {
    clamp_budget_inner(budget, caps)
}

fn clamp_budget_inner(budget: u32, caps: &ThinkingCapabilities) -> u32 {
    if let Some((min, max)) = caps.budget_range {
        budget.clamp(min, max)
    } else {
        budget
    }
}

/// 在 caps.effort_levels 中找距离请求档位最近的支持档（rank 差最小，平局取较高）。
pub fn pick_nearest_supported(requested: EffortLevel, supported: &[EffortLevel]) -> EffortLevel {
    let req_rank = requested.rank();
    *supported
        .iter()
        .min_by_key(|lv| {
            let d = lv.rank().abs_diff(req_rank);
            // 平局优先取较高档位（更安全，避免欠思考）
            (d, u8::MAX - lv.rank())
        })
        .unwrap_or(&EffortLevel::Medium)
}

fn nearest_effort_for_budget(budget: u32, supported: &[EffortLevel]) -> EffortLevel {
    *supported
        .iter()
        .min_by_key(|lv| lv.default_budget_tokens().abs_diff(budget))
        .unwrap_or(&EffortLevel::Medium)
}

/// 返回 caps 支持的最高档位
pub fn highest_supported_effort(caps: &ThinkingCapabilities) -> Option<EffortLevel> {
    caps.effort_levels.iter().max_by_key(|l| l.rank()).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::MultiTurnReplay;

    fn caps_anthropic_opus_4_7() -> ThinkingCapabilities {
        ThinkingCapabilities {
            supported_modes: vec![ThinkingModeKind::AdaptiveEffort],
            default_mode: Some(ThinkingModeKind::AdaptiveEffort),
            effort_levels: vec![
                EffortLevel::Low, EffortLevel::Medium, EffortLevel::High,
                EffortLevel::Max, EffortLevel::XHigh,
            ],
            budget_range: None,
            multi_turn_replay: MultiTurnReplay::None,
        }
    }

    fn caps_anthropic_sonnet_4_5() -> ThinkingCapabilities {
        ThinkingCapabilities {
            supported_modes: vec![ThinkingModeKind::ExtendedBudget],
            default_mode: Some(ThinkingModeKind::ExtendedBudget),
            effort_levels: vec![],
            budget_range: Some((1024, 64000)),
            multi_turn_replay: MultiTurnReplay::None,
        }
    }

    /// 注意：这是测试用的**合成** fixture，仅模拟 "EnabledToggle-only + 无 effort 档位"
    /// 的代码路径。**不再**代表 DeepSeek 真实 caps（D1 起 catalog 已声明 DS 支持
    /// AdaptiveEffort + 全 5 档 effort_levels；服务端 alias low/medium→high, xhigh→max
    /// 由 deepseek.rs::deepseek_effort_clamp 在 client 侧镜像处理）。
    fn caps_deepseek_openai() -> ThinkingCapabilities {
        ThinkingCapabilities {
            supported_modes: vec![ThinkingModeKind::EnabledToggle],
            default_mode: Some(ThinkingModeKind::EnabledToggle),
            effort_levels: vec![],
            budget_range: None,
            multi_turn_replay: MultiTurnReplay::ReasoningContent,
        }
    }

    fn caps_gpt_5() -> ThinkingCapabilities {
        ThinkingCapabilities {
            supported_modes: vec![ThinkingModeKind::AdaptiveEffort],
            default_mode: Some(ThinkingModeKind::AdaptiveEffort),
            effort_levels: vec![
                EffortLevel::Minimal, EffortLevel::Low, EffortLevel::Medium, EffortLevel::High,
            ],
            budget_range: None,
            multi_turn_replay: MultiTurnReplay::None,
        }
    }

    fn caps_gemini_pro() -> ThinkingCapabilities {
        ThinkingCapabilities {
            supported_modes: vec![ThinkingModeKind::BudgetInt],
            default_mode: Some(ThinkingModeKind::BudgetInt),
            effort_levels: vec![],
            budget_range: Some((128, 32_768)),
            multi_turn_replay: MultiTurnReplay::None,
        }
    }

    #[test]
    fn test_off_passes_through() {
        let r = validate_intent_against_caps(&ThinkingIntent::Off, &caps_anthropic_opus_4_7());
        assert_eq!(r.accepted_intent, ThinkingIntent::Off);
        assert!(r.notes.is_empty());
    }

    #[test]
    fn test_adaptive_native_on_opus_4_7() {
        let r = validate_intent_against_caps(&ThinkingIntent::Adaptive, &caps_anthropic_opus_4_7());
        assert_eq!(r.accepted_intent, ThinkingIntent::Adaptive);
        assert!(r.notes.is_empty());
    }

    #[test]
    fn test_adaptive_falls_back_on_sonnet_4_5() {
        // Sonnet 4.5 不支持 adaptive → 应降级为 budget=64000
        let r = validate_intent_against_caps(&ThinkingIntent::Adaptive, &caps_anthropic_sonnet_4_5());
        assert!(matches!(r.accepted_intent, ThinkingIntent::Budget(64000)));
        assert!(matches!(
            r.notes.first(),
            Some(DegradeNote::AdaptiveUnsupportedFellBackToBudget(64000))
        ));
    }

    #[test]
    fn test_adaptive_falls_back_on_deepseek() {
        // DeepSeek OpenAI 格式只支持 enabled toggle
        let r = validate_intent_against_caps(&ThinkingIntent::Adaptive, &caps_deepseek_openai());
        assert!(matches!(
            r.accepted_intent,
            ThinkingIntent::Effort(EffortLevel::High)
        ));
        assert_eq!(r.notes.len(), 1);
    }

    #[test]
    fn test_minimal_effort_only_supported_on_gpt5() {
        // GPT-5 支持 minimal
        let r = validate_intent_against_caps(
            &ThinkingIntent::Effort(EffortLevel::Minimal),
            &caps_gpt_5(),
        );
        assert_eq!(r.accepted_intent, ThinkingIntent::Effort(EffortLevel::Minimal));
        assert!(r.notes.is_empty());

        // Opus 4.7 不支持 minimal → 应降级到最近档位 Low
        let r = validate_intent_against_caps(
            &ThinkingIntent::Effort(EffortLevel::Minimal),
            &caps_anthropic_opus_4_7(),
        );
        assert_eq!(r.accepted_intent, ThinkingIntent::Effort(EffortLevel::Low));
    }

    #[test]
    fn test_effort_converted_to_budget_on_sonnet_4_5() {
        let r = validate_intent_against_caps(
            &ThinkingIntent::Effort(EffortLevel::High),
            &caps_anthropic_sonnet_4_5(),
        );
        // High default = 16384，落在 (1024, 64000) 范围内
        assert!(matches!(r.accepted_intent, ThinkingIntent::Budget(16384)));
    }

    #[test]
    fn test_effort_dropped_on_deepseek_openai() {
        let r = validate_intent_against_caps(
            &ThinkingIntent::Effort(EffortLevel::Low),
            &caps_deepseek_openai(),
        );
        // 档位丢失但保留 enabled
        assert!(matches!(r.accepted_intent, ThinkingIntent::Effort(EffortLevel::High)));
        assert!(matches!(
            r.notes.first(),
            Some(DegradeNote::EffortDroppedForEnabledToggle(EffortLevel::Low))
        ));
    }

    #[test]
    fn test_budget_clamped_to_range() {
        // Sonnet 4.5 budget_range = (1024, 64000)；请求 100 → 1024
        let r = validate_intent_against_caps(
            &ThinkingIntent::Budget(100),
            &caps_anthropic_sonnet_4_5(),
        );
        assert!(matches!(r.accepted_intent, ThinkingIntent::Budget(1024)));
        assert!(r.notes.iter().any(|n| matches!(n, DegradeNote::BudgetClamped { from: 100, to: 1024 })));

        // 请求 999999 → 64000
        let r = validate_intent_against_caps(
            &ThinkingIntent::Budget(999_999),
            &caps_anthropic_sonnet_4_5(),
        );
        assert!(matches!(r.accepted_intent, ThinkingIntent::Budget(64_000)));
    }

    #[test]
    fn test_budget_converts_to_effort_on_adaptive_only_model() {
        // Opus 4.7 不接受 budget → 找最近 effort 档位
        // budget=20000 距离 High(16384)=3616 < Max(32768)=12768 → High
        let r = validate_intent_against_caps(
            &ThinkingIntent::Budget(20_000),
            &caps_anthropic_opus_4_7(),
        );
        assert!(matches!(
            r.accepted_intent,
            ThinkingIntent::Effort(EffortLevel::High)
        ));
    }

    #[test]
    fn test_budget_native_on_gemini() {
        let r = validate_intent_against_caps(
            &ThinkingIntent::Budget(8192),
            &caps_gemini_pro(),
        );
        assert!(matches!(r.accepted_intent, ThinkingIntent::Budget(8192)));
        assert!(r.notes.is_empty());
    }

    #[test]
    fn test_no_thinking_capability_forces_off() {
        let caps = ThinkingCapabilities::none();
        // 即使用户请求 high，也强制 Off
        let r = validate_intent_against_caps(
            &ThinkingIntent::Effort(EffortLevel::High),
            &caps,
        );
        assert_eq!(r.accepted_intent, ThinkingIntent::Off);
        assert_eq!(r.notes, vec![DegradeNote::ModelDoesNotSupportThinking]);
    }

    #[test]
    fn test_pick_nearest_supported_tie_picks_higher() {
        // 平局取较高档位（更安全策略）
        let supported = vec![EffortLevel::Low, EffortLevel::High];
        // Medium(rank=2) 距离 Low(rank=1)=1，距离 High(rank=3)=1 → 取 High
        assert_eq!(pick_nearest_supported(EffortLevel::Medium, &supported), EffortLevel::High);
    }
}
