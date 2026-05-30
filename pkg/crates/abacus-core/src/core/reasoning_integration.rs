//! Reasoning 子系统集成层
//!
//! ## 职责
//! 将 `reasoning::consistency` 和 `reasoning::tot` 接入 TurnPipeline Phase 4。
//! 提供配置、自动触发逻辑、置信度估算。
//!
//! ## 引用关系
//! - 被 `TurnPipeline::maybe_apply_reasoning()` 调用
//! - 消费 `crate::reasoning::{consistency, tot}`
//! - 读取 `CoreLoop.reasoning_config`
//!
//! ## 触发时机
//! Phase 4 LLM 首次 response 返回后：
//! 1. estimate_confidence(response) < threshold → Self-Consistency
//! 2. task_kind ∈ tot_auto_task_kinds → Tree of Thoughts

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::llm::{LlmProvider, LlmRequest};
use crate::reasoning::consistency::{self, ConsistencyConfig, ConsistencyResult};
use crate::reasoning::tot::{self, SearchStrategy, ToTConfig, ToTResult};

// ─── ReasoningConfig ───────────────────────────────────────────────────────

/// Reasoning 子系统配置
///
/// ## 生命周期
/// - 创建：CoreLoop::new() 从 config.toml [reasoning] 段加载
/// - 读取：TurnPipeline Phase 4 每轮检查
/// - 修改：/config set reasoning.* 运行时动态修改
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningConfig {
    /// 自动触发开关（默认 true）
    /// false 时仅 /reason 命令手动触发
    pub auto_trigger: bool,

    /// Self-Consistency 置信度阈值
    /// LLM response 低于此值时自动触发多路采样（默认 0.6）
    pub confidence_threshold: f64,

    /// Self-Consistency 并行采样次数（默认 3）
    pub consistency_samples: usize,

    /// Self-Consistency 采样温度（默认 0.7）
    pub consistency_temperature: f64,

    /// Tree of Thoughts 自动触发的 task_kind 列表
    pub tot_auto_task_kinds: Vec<String>,

    /// ToT 最大搜索深度（默认 3）
    pub tot_max_depth: u32,

    /// ToT 每层候选分支数（默认 5）
    pub tot_branch_factor: u32,

    /// 单轮 reasoning 额外 token 预算上限（防止过度消耗）
    pub max_extra_tokens_per_turn: u32,
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self {
            auto_trigger: true,
            confidence_threshold: 0.6,
            consistency_samples: 3,
            consistency_temperature: 0.7,
            tot_auto_task_kinds: vec![
                "architecture".into(),
                "planning".into(),
            ],
            tot_max_depth: 3,
            tot_branch_factor: 5,
            max_extra_tokens_per_turn: 8000,
        }
    }
}

// ─── Confidence Estimation ─────────────────────────────────────────────────

/// 不确定性标记词（中英文）
const UNCERTAINTY_MARKERS: &[&str] = &[
    // 中文
    "不确定", "可能", "也许", "大概", "不太清楚", "需要确认",
    // 英文
    "I'm not sure", "might be", "perhaps", "maybe", "possibly",
    "I think", "not certain", "unclear",
];

/// 从 LLM response 估算置信度
///
/// ## 策略
/// - 检测不确定性标记词 → 0.4
/// - 回复过短（< 50 字符）→ 0.5
/// - 回复含自我矛盾（"但是"/"however" 后接否定）→ 0.45
/// - 正常回复 → 0.8
///
/// ## 局限性
/// 这是启发式估算，非精确度量。后续可接入 LLM-as-Judge 做二次评估。
pub fn estimate_confidence(response_text: &str) -> f64 {
    if response_text.is_empty() {
        return 0.3;
    }

    // 检测不确定性标记
    let has_uncertainty = UNCERTAINTY_MARKERS.iter()
        .any(|marker| response_text.contains(marker));

    if has_uncertainty {
        return 0.4;
    }

    // 过短回复
    if response_text.len() < 50 {
        return 0.5;
    }

    // 自我矛盾检测（简单启发式）
    let contradiction_markers = ["但是", "不过", "however", "although", "but actually"];
    let has_contradiction = contradiction_markers.iter()
        .any(|m| response_text.contains(m));

    if has_contradiction && response_text.len() < 200 {
        return 0.45;
    }

    0.8
}

// ─── Reasoning Trigger ─────────────────────────────────────────────────────

/// Reasoning 触发结果
pub enum ReasoningOutcome {
    /// 不触发（置信度足够或配置关闭）
    Skip,
    /// Self-Consistency 结果（替换原始 response）
    ConsistencyEnhanced(ConsistencyResult),
    /// ToT 规划结果（注入后续执行上下文，不替换 response）
    TotPlan(ToTResult),
}

/// 尝试应用 Reasoning 增强
///
/// ## 调用时机
/// TurnPipeline Phase 4，LLM 首次 response 返回后。
///
/// ## 返回
/// - `Skip`：不触发（大多数情况）
/// - `ConsistencyEnhanced`：触发了 Self-Consistency，应替换 response
/// - `TotPlan`：触发了 ToT，应注入 plan 到后续 context
pub async fn try_reasoning_enhancement(
    config: &ReasoningConfig,
    provider: Arc<dyn LlmProvider>,
    request: &LlmRequest,
    response_text: &str,
    task_kind: &str,
    user_input: &str,
    force: bool,  // /reason 命令时 force=true
) -> ReasoningOutcome {
    // 配置关闭且非强制 → 跳过
    if !config.auto_trigger && !force {
        return ReasoningOutcome::Skip;
    }

    let confidence = estimate_confidence(response_text);

    // Self-Consistency 路径
    if confidence < config.confidence_threshold || force {
        let sc_config = ConsistencyConfig {
            n: config.consistency_samples,
            temperature: config.consistency_temperature,
        };

        match consistency::consistent_sample(provider.clone(), request, &sc_config).await {
            Ok(result) => {
                // 只有当 consistency 结果更好时才采用
                if result.confidence > confidence || force {
                    return ReasoningOutcome::ConsistencyEnhanced(result);
                }
            }
            Err(_) => {
                // Consistency 采样失败，静默降级（不阻断主流程）
            }
        }
    }

    // ToT 路径（Architecture/Planning 任务）
    if config.tot_auto_task_kinds.iter().any(|k| k == task_kind) || force {
        let tot_config = ToTConfig {
            max_depth: config.tot_max_depth,
            branch_factor: config.tot_branch_factor,
            strategy: SearchStrategy::Beam { k: 3 },
            lang: "en",
        };

        let result = tot::tot_plan(provider, request, user_input, &tot_config).await;

        if result.found_sure_path {
            return ReasoningOutcome::TotPlan(result);
        }
    }

    ReasoningOutcome::Skip
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_confidence_empty() {
        assert_eq!(estimate_confidence(""), 0.3);
    }

    #[test]
    fn test_estimate_confidence_uncertain_zh() {
        assert_eq!(estimate_confidence("这个问题我不确定答案是什么"), 0.4);
    }

    #[test]
    fn test_estimate_confidence_uncertain_en() {
        assert_eq!(estimate_confidence("I'm not sure about the exact implementation"), 0.4);
    }

    #[test]
    fn test_estimate_confidence_short() {
        assert_eq!(estimate_confidence("42"), 0.5);
    }

    #[test]
    fn test_estimate_confidence_normal() {
        let text = "The function processes the input by first validating the parameters, \
                    then applying the transformation algorithm, and finally returning the result \
                    with proper error handling for edge cases.";
        assert_eq!(estimate_confidence(text), 0.8);
    }

    #[test]
    fn test_estimate_confidence_contradiction_short() {
        // To reach the contradiction branch (0.45), text must:
        // 1. NOT contain UNCERTAINTY_MARKERS (e.g. "可能", "maybe")
        // 2. Be >= 50 chars (to bypass the short-response check)
        // 3. Contain a contradiction marker ("但是", "however", "but actually", etc.)
        // 4. Be < 200 chars
        let text = "The implementation uses a recursive approach however the iterative one is better suited";
        assert!(text.len() >= 50 && text.len() < 200);
        assert_eq!(estimate_confidence(text), 0.45);
    }

    #[test]
    fn test_default_config() {
        let config = ReasoningConfig::default();
        assert!(config.auto_trigger);
        assert_eq!(config.confidence_threshold, 0.6);
        assert_eq!(config.consistency_samples, 3);
        assert_eq!(config.tot_auto_task_kinds, vec!["architecture", "planning"]);
    }
}
