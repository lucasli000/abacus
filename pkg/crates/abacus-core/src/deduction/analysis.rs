//! Analysis algorithms — O1 invariant extraction, O3 signal decomposition,
//! A5 observer contamination detection, context degradation prediction.
//!
//! ## 场景
//! 被 DeductionEngine 调用，消费 MetricStore 的时序数据，产出分析报告。
//!
//! ## 依赖
//! - `series.rs`: MetricStore 时序数据
//! - `regex`: 模式匹配
//!
//! ## 引用关系
//! - 被 `DeductionEngine::analyze()` 调用
//! - 不持有状态，纯函数式分析

use std::collections::HashMap;

use crate::deduction::series::{ContextUsagePoint, PromptStructurePoint, ToolMetricPoint};

// ─── O1: 不变量提取 ────────────────────────────────────────────────────────

/// 对工具的历史数据提取行为不变量
pub fn extract_invariants(history: &[ToolMetricPoint]) -> Vec<String> {
    let mut invariants = Vec::new();

    if history.len() < 3 {
        invariants.push("data_insufficient: 采样不足，无法提取可靠不变量".into());
        return invariants;
    }

    let adoption_vals: Vec<f64> = history.iter().map(|p| p.adoption_rate).collect();
    let success_vals: Vec<f64> = history.iter().map(|p| p.success_rate).collect();
    let latency_vals: Vec<f64> = history.iter().map(|p| p.avg_latency_ms).collect();

    // 采纳率稳定性
    let adoption_range = range(&adoption_vals);
    if adoption_range < 0.1 {
        invariants.push(format!("adoption_stable: 采纳率稳定在 {:.0}%±{:.0}%",
            mean(&adoption_vals) * 100.0, adoption_range * 100.0));
    } else {
        invariants.push(format!("adoption_volatile: 采纳率波动幅度 {:.0}%", adoption_range * 100.0));
    }

    // 成功率基线
    let success_min = success_vals.iter().cloned().fold(f64::MAX, f64::min);
    if success_min > 0.9 {
        invariants.push("success_high: 工具可靠性高，成功率始终 > 90%".into());
    } else if success_min < 0.5 {
        invariants.push("success_low: 工具可靠性低，存在成功率低于 50% 的记录".into());
    }

    // 延迟稳定性
    let lat_range = range(&latency_vals);
    let lat_mean = mean(&latency_vals);
    if lat_mean > 0.0 && lat_range / lat_mean < 0.2 {
        invariants.push("latency_stable: 延迟稳定，波动 < 20%".into());
    }

    // 评分趋势方向
    let scores: Vec<f64> = history.iter().map(|p| p.composite_score).collect();
    let slope = linear_trend(&scores);
    if slope > 0.02 {
        invariants.push(format!("score_improving: 综合评分呈上升趋势 (slope={:.4})", slope));
    } else if slope < -0.02 {
        invariants.push(format!("score_declining: 综合评分呈下降趋势 (slope={:.4})", slope));
    } else {
        invariants.push("score_stable: 综合评分无明显趋势".into());
    }

    invariants
}

// ─── O3: 信号分解 ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SignalDecomposition {
    pub slow_trend: f64,    // 结构层：整体趋势方向 + 幅度
    pub medium_cycle: f64,  // 周期层：近几轮的波动幅度
    pub fast_noise: f64,    // 噪声层：不可预测的波动
    pub explanation: String,
}

/// 将工具指标分解为慢变量（趋势）、中变量（周期）、快变量（噪声）
pub fn decompose_signal(history: &[ToolMetricPoint]) -> SignalDecomposition {
    if history.len() < 5 {
        return SignalDecomposition {
            slow_trend: 0.0, medium_cycle: 0.0, fast_noise: 0.0,
            explanation: "数据不足（<5点），无法可靠分解".into(),
        };
    }

    // 取 composite_score 作为主信号
    let vals: Vec<f64> = history.iter().map(|p| p.composite_score).collect();
    let n = vals.len();

    // 慢变量：整体线性趋势
    let slow = linear_trend(&vals);

    // 快变量：残差的标准差（去趋势后）
    let mean_v = mean(&vals);
    let residuals: Vec<f64> = vals.iter().enumerate().map(|(i, v)| {
        let trend_val = mean_v + slow * (i as f64 - n as f64 / 2.0);
        v - trend_val
    }).collect();
    let fast = std_dev(&residuals);

    // 中变量：近 3 点的局部波动 vs 全局波动
    let recent = &vals[n.saturating_sub(3)..];
    let recent_range = range(recent);
    let global_range = range(&vals);
    let medium = if global_range > 0.0 {
        (recent_range / global_range).min(1.0)
    } else {
        0.0
    };

    let explanation = if slow.abs() > 0.05 {
        format!("趋势明显 (slope={:.3})，建议关注方向变化", slow)
    } else if fast > 0.1 {
        format!("噪声较大 (σ={:.3})，单点数据参考价值有限", fast)
    } else {
        "信号稳定，趋势和噪声均在正常范围".into()
    };

    SignalDecomposition { slow_trend: slow, medium_cycle: medium, fast_noise: fast, explanation }
}

// ─── A5: 观察者污染检测 ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ContaminationAlert {
    pub tool_id: String,
    pub adoption_rate: f64,
    pub success_rate: f64,
    /// adoption - success 差值（正 = LLM 偏爱超出实际效用）
    pub divergence: f64,
    pub since_turns: u32,
    pub severity: ContaminationSeverity,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContaminationSeverity {
    /// 差值 < 0.1，正常
    None,
    /// 0.1 ≤ 差值 < 0.2，轻微偏离
    Mild,
    /// 0.2 ≤ 差值 < 0.35，需要关注
    Watch,
    /// 差值 ≥ 0.35，评分可能已失真
    Critical,
}

impl ContaminationSeverity {
    pub fn label(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Mild => "mild",
            Self::Watch => "watch",
            Self::Critical => "critical",
        }
    }
}

/// 检测观察者污染：adoption_rate 与 success_rate 的持续偏离
///
/// ## 原理
/// 污染信号 = adoption_rate - success_rate（综合考虑 trend 方向）
/// - 正值且持续扩大 → LLM 被过度推荐影响，选了不该选的工具
/// - 负值且持续 → 工具效用 > LLM 采纳意愿（可能定义不清晰）
pub fn detect_contamination(history: &[ToolMetricPoint]) -> Vec<ContaminationAlert> {
    if history.len() < 3 {
        return Vec::new();
    }

    let mut alerts = Vec::new();

    // 按 tool_id 分组
    let mut by_tool: HashMap<&str, Vec<&ToolMetricPoint>> = HashMap::new();
    for p in history {
        by_tool.entry(&p.tool_id.0).or_default().push(p);
    }

    for (tool_id, points) in &by_tool {
        if points.len() < 3 { continue; }

        let sorted = {
            let mut s = points.clone();
            s.sort_by_key(|p| p.timestamp_ms);
            s
        };

        let recent: Vec<&&ToolMetricPoint> = sorted.iter().rev().take(5).collect();
        let adoption = recent.iter().map(|p| p.adoption_rate).sum::<f64>() / recent.len() as f64;
        let success = recent.iter().map(|p| p.success_rate).sum::<f64>() / recent.len() as f64;
        let divergence = adoption - success;

        // 检查 trend 方向：如果 adoption 在上升但 success 没跟上 → 更严重
        let adopt_slope = linear_trend(&sorted.iter().map(|p| p.adoption_rate).collect::<Vec<_>>());
        let success_slope = linear_trend(&sorted.iter().map(|p| p.success_rate).collect::<Vec<_>>());

        let severity = if divergence > 0.35 {
            ContaminationSeverity::Critical
        } else if divergence > 0.20 {
            ContaminationSeverity::Watch
        } else if divergence > 0.10 {
            ContaminationSeverity::Mild
        } else {
            ContaminationSeverity::None
        };

        if severity != ContaminationSeverity::None {
            // 计算持续轮数
            let divergence_count = sorted.iter().rev()
                .take_while(|p| (p.adoption_rate - p.success_rate) > 0.10)
                .count() as u32;

            alerts.push(ContaminationAlert {
                tool_id: tool_id.to_string(),
                adoption_rate: adoption,
                success_rate: success,
                divergence,
                since_turns: divergence_count,
                severity,
            });
        }

        // 额外：adoption_rate 上升但 success_rate 下降 → 强烈污染信号
        if adopt_slope > 0.02 && success_slope < -0.01 {
            alerts.push(ContaminationAlert {
                tool_id: tool_id.to_string(),
                adoption_rate: adoption,
                success_rate: success,
                divergence,
                since_turns: 0,
                severity: ContaminationSeverity::Watch,
            });
        }
    }

    alerts
}

// ─── Context 退化预测 ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DegradationPrediction {
    pub current_usage_pct: f64,
    pub growth_rate: f64,         // 每轮增长百分比点
    pub turns_to_85pct: i64,      // 预计多少轮后达到 85% 压缩阈值
    pub turns_to_95pct: i64,      // 预计多少轮后达到 95% 强制丢弃
    pub acceleration: f64,        // 增长率加速度（二阶导）
    pub action: DegradationAction,
}

#[derive(Debug, Clone)]
pub enum DegradationAction {
    Normal,
    PrepareCompress,
    ImmediateCompress,
    ForceDiscard,
}

impl DegradationAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::PrepareCompress => "prepare_compress",
            Self::ImmediateCompress => "immediate_compress",
            Self::ForceDiscard => "force_discard",
        }
    }
}

/// 预测 context 退化趋势
pub fn predict_context_degradation(history: &[ContextUsagePoint]) -> DegradationPrediction {
    if history.len() < 3 {
        return DegradationPrediction {
            current_usage_pct: history.last().map(|p| p.usage_pct).unwrap_or(0.0),
            growth_rate: 0.0, turns_to_85pct: 999, turns_to_95pct: 999,
            acceleration: 0.0, action: DegradationAction::Normal,
        };
    }

    let sorted = {
        let mut s = history.to_vec();
        s.sort_by_key(|p| p.turn_number);
        s
    };

    let current = sorted.last().map(|p| p.usage_pct).unwrap_or(0.0);
    let usages: Vec<f64> = sorted.iter().map(|p| p.usage_pct).collect();
    let growth = linear_trend(&usages);

    // 二阶导（加速度）：前后半段增长率的差值
    let mid = usages.len() / 2;
    let first_half = linear_trend(&usages[..mid]);
    let second_half = linear_trend(&usages[mid..]);
    let acceleration = second_half - first_half;

    // 预计到达阈值
    let turns_to_85 = if growth > 0.0 {
        ((85.0 - current) / growth).ceil() as i64
    } else {
        999
    };
    let turns_to_95 = if growth > 0.0 {
        ((95.0 - current) / growth).ceil() as i64
    } else {
        999
    };

    let action = if current >= 95.0 {
        DegradationAction::ForceDiscard
    } else if current >= 85.0 || turns_to_85 <= 0 {
        DegradationAction::ImmediateCompress
    } else if turns_to_85 <= 3 || current >= 70.0 {
        DegradationAction::PrepareCompress
    } else {
        DegradationAction::Normal
    };

    DegradationPrediction {
        current_usage_pct: current,
        growth_rate: growth,
        turns_to_85pct: turns_to_85.max(0),
        turns_to_95pct: turns_to_95.max(0),
        acceleration,
        action,
    }
}

// ─── Prompt 影响追踪 ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PromptImpactDelta {
    pub layer_count_changed: bool,
    pub tool_count_changed: bool,
    pub tool_set_changed: bool,
    pub thinking_toggled: bool,
    pub description: String,
}

/// Compare two consecutive prompt structure snapshots to detect changes.
pub fn detect_prompt_change(
    prev: &PromptStructurePoint,
    curr: &PromptStructurePoint,
) -> PromptImpactDelta {
    let layer_count_changed = prev.layer_count != curr.layer_count;
    let tool_count_changed = prev.tool_count != curr.tool_count;
    let tool_set_changed = prev.tool_set_hash != curr.tool_set_hash
        && curr.tool_set_hash != 0;
    let thinking_toggled = prev.has_thinking != curr.has_thinking;

    let mut parts = Vec::new();
    if layer_count_changed { parts.push(format!("层数 {}→{}", prev.layer_count, curr.layer_count)); }
    if tool_count_changed { parts.push(format!("工具数 {}→{}", prev.tool_count, curr.tool_count)); }
    if tool_set_changed { parts.push("工具集变化".into()); }
    if thinking_toggled { parts.push(if curr.has_thinking { "开启 thinking".into() } else { "关闭 thinking".into() }); }

    let description = if parts.is_empty() {
        "Prompt 结构未变化".into()
    } else {
        format!("Prompt 变更: {}", parts.join(", "))
    };

    PromptImpactDelta {
        layer_count_changed, tool_count_changed, tool_set_changed,
        thinking_toggled: thinking_toggled && curr.has_thinking,
        description,
    }
}

/// 分析 Prompt 变更后工具的采纳率是否异常
pub fn detect_prompt_impact(
    prompt_prev: &PromptStructurePoint,
    prompt_curr: &PromptStructurePoint,
    before_metrics: &[ToolMetricPoint],
    after_metrics: &[ToolMetricPoint],
) -> Vec<String> {
    let delta = detect_prompt_change(prompt_prev, prompt_curr);
    if !delta.tool_count_changed && !delta.tool_set_changed && !delta.thinking_toggled {
        return vec!["Prompt 无显著变化，跳过影响分析".into()];
    }

    let mut findings = Vec::new();
    findings.push(delta.description.clone());

    // 比较变前后的工具采纳率均值
    if !before_metrics.is_empty() && !after_metrics.is_empty() {
        let before_adopt = mean(&before_metrics.iter().map(|p| p.adoption_rate).collect::<Vec<_>>());
        let after_adopt = mean(&after_metrics.iter().map(|p| p.adoption_rate).collect::<Vec<_>>());
        let diff = after_adopt - before_adopt;
        if diff.abs() > 0.05 {
            findings.push(format!(
                "工具采纳率变化: {:.0}% → {:.0}% ({:+.0}%)",
                before_adopt * 100.0, after_adopt * 100.0, diff * 100.0
            ));
        }
    }

    findings
}

// ─── 跨 session 模式提取 ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CrossSessionPattern {
    pub description: String,
    pub pattern_type: &'static str,
    pub confidence: f64,
    pub evidence: String,
}

/// 从全局工具指标中提取跨 session 模式
///
/// ## 检测的模式
/// 1. 工具共现倾向（哪些工具经常同时被调用）
/// 2. session 后半段行为退化（某些工具在 context 高时表现差）
/// 3. 时序依赖（工具 A 调用后总是紧跟工具 B）
pub fn extract_cross_session_patterns(all_metrics: &[ToolMetricPoint]) -> Vec<CrossSessionPattern> {
    if all_metrics.is_empty() {
        return Vec::new();
    }

    let mut patterns = Vec::new();

    // 模式1: 按 session_id 分组，统计工具共现
    let mut by_session: HashMap<&str, Vec<&ToolMetricPoint>> = HashMap::new();
    for p in all_metrics {
        by_session.entry(&p.session_id).or_default().push(p);
    }

    for (session_id, points) in &by_session {
        if points.len() < 3 { continue; }

        let tools: std::collections::HashSet<&str> =
            points.iter().map(|p| p.tool_id.0.as_str()).collect();
        if tools.len() >= 3 {
            patterns.push(CrossSessionPattern {
                description: format!("session {} 使用了 {} 种不同工具", session_id, tools.len()),
                pattern_type: "tool_diversity",
                confidence: 0.7,
                evidence: format!("工具: {:?}", tools.iter().take(5).collect::<Vec<_>>()),
            });
        }

        // 检查：session 后半段成功率是否下降
        let mid = points.len() / 2;
        let first_half_success = mean(&points[..mid].iter().map(|p| p.success_rate).collect::<Vec<_>>());
        let second_half_success = mean(&points[mid..].iter().map(|p| p.success_rate).collect::<Vec<_>>());
        if first_half_success > second_half_success + 0.1 {
            patterns.push(CrossSessionPattern {
                description: "session 后半段工具成功率显著下降，可能与 context 压力相关".into(),
                pattern_type: "session_fatigue",
                confidence: 0.6,
                evidence: format!("前半段 {:.0}% → 后半段 {:.0}%",
                    first_half_success * 100.0, second_half_success * 100.0),
            });
        }
    }

    // 模式3: 评估整体的 adoption vs success 偏差（多工具平均污染）
    let total_adoption: f64 = all_metrics.iter().map(|p| p.adoption_rate).sum();
    let total_success: f64 = all_metrics.iter().map(|p| p.success_rate).sum();
    let count = all_metrics.len() as f64;
    if count > 0.0 {
        let avg_adoption = total_adoption / count;
        let avg_success = total_success / count;
        if avg_adoption > avg_success + 0.15 {
            patterns.push(CrossSessionPattern {
                description: "全局 adoption_rate 显著高于 success_rate，可能存在系统性评分膨胀".into(),
                pattern_type: "systematic_contamination",
                confidence: 0.65,
                evidence: format!("平均 adoption={:.0}%, success={:.0}%",
                    avg_adoption * 100.0, avg_success * 100.0),
            });
        }
    }

    patterns
}

// ─── 工具函数 ───────────────────────────────────────────────────────────────

fn mean(vals: &[f64]) -> f64 {
    if vals.is_empty() { return 0.0; }
    vals.iter().sum::<f64>() / vals.len() as f64
}

fn range(vals: &[f64]) -> f64 {
    if vals.is_empty() { return 0.0; }
    let min = vals.iter().cloned().fold(f64::MAX, f64::min);
    let max = vals.iter().cloned().fold(f64::MIN, f64::max);
    max - min
}

fn std_dev(vals: &[f64]) -> f64 {
    if vals.len() < 2 { return 0.0; }
    let m = mean(vals);
    let variance = vals.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (vals.len() - 1) as f64;
    variance.sqrt()
}

fn linear_trend(vals: &[f64]) -> f64 {
    let n = vals.len() as f64;
    if n < 2.0 { return 0.0; }
    let sum_x: f64 = (0..vals.len()).map(|i| i as f64).sum();
    let sum_y: f64 = vals.iter().sum();
    let sum_xy: f64 = vals.iter().enumerate().map(|(i, v)| i as f64 * v).sum();
    let sum_xx: f64 = (0..vals.len()).map(|i| (i as f64).powi(2)).sum();
    let slope = (n * sum_xy - sum_x * sum_y) / (n * sum_xx - sum_x.powi(2));
    if slope.is_nan() || slope.is_infinite() { 0.0 } else { slope }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deduction::series::ToolMetricPoint;
    use abacus_types::ToolId;

    fn make_metric(tool: &str, turn: u32, adopt: f64, success: f64, score: f64) -> ToolMetricPoint {
        ToolMetricPoint {
            tool_id: ToolId(tool.into()), turn_number: turn, session_id: "s1".into(),
            timestamp_ms: turn as i64 * 1000,
            adoption_rate: adopt, success_rate: success, trend: 0.0,
            composite_score: score, visibility_tier: "A".into(),
            opportunities: 10, invocations: (adopt * 10.0) as u64,
            successes: (success * 10.0) as u64, avg_latency_ms: 50.0,
        }
    }

    #[test]
    fn test_contamination_detection() {
        let history = vec![
            make_metric("filengine_fs_read", 1, 0.9, 0.5, 0.70),
            make_metric("filengine_fs_read", 2, 0.9, 0.4, 0.70),
            make_metric("filengine_fs_read", 3, 0.9, 0.5, 0.70),
            make_metric("filengine_fs_read", 4, 0.9, 0.4, 0.70),
        ];
        let alerts = detect_contamination(&history);
        let has_fs = alerts.iter().any(|a| a.tool_id == "filengine_fs_read" && a.severity == ContaminationSeverity::Critical);
        assert!(has_fs, "should detect critical contamination: adoption={}, success={}", 0.9, 0.45);
    }

    #[test]
    fn test_no_contamination_when_aligned() {
        let history = vec![
            make_metric("filengine_fs_read", 1, 0.8, 0.85, 0.75),
            make_metric("filengine_fs_read", 2, 0.8, 0.82, 0.75),
            make_metric("filengine_fs_read", 3, 0.8, 0.83, 0.75),
        ];
        let alerts = detect_contamination(&history);
        assert!(alerts.is_empty(), "should not alert when adoption ≈ success");
    }

    #[test]
    fn test_signal_decomposition() {
        let history: Vec<ToolMetricPoint> = (0..10).map(|i| {
            let score = 0.5 + i as f64 * 0.04;
            make_metric("test", i, score, score, score)
        }).collect();
        let signal = decompose_signal(&history);
        assert!(signal.slow_trend > 0.02, "rising scores should have positive trend");
    }

    #[test]
    fn test_context_prediction() {
        let history: Vec<ContextUsagePoint> = (0..10).map(|i| {
            ContextUsagePoint {
                turn_number: i, session_id: "s1".into(), timestamp_ms: i as i64 * 1000,
                usage_pct: 40.0 + i as f64 * 3.0, max_tokens: 128_000,
                current_tokens: (40000 + i * 3000) as usize,
                was_compressed: false,
            }
        }).collect();
        let pred = predict_context_degradation(&history);
        assert!(pred.growth_rate > 0.0);
        assert!(pred.turns_to_85pct > 0, "turns_to_85 should be positive, got {}", pred.turns_to_85pct);
    }

    #[test]
    fn test_cross_session_patterns() {
        let all = vec![
            make_metric("filengine_fs_read", 1, 0.8, 0.9, 0.7),
            make_metric("filengine_fs_read", 2, 0.8, 0.9, 0.7),
            make_metric("filengine_fs_read", 3, 0.8, 0.9, 0.7),
            make_metric("filengine_web_fetch", 1, 0.9, 0.5, 0.6),
            make_metric("filengine_web_fetch", 2, 0.9, 0.4, 0.6),
            make_metric("filengine_web_fetch", 3, 0.9, 0.5, 0.6),
        ];
        let patterns = extract_cross_session_patterns(&all);
        // Should detect systematic contamination (web.fetch adoption >> success)
        let has_sys = patterns.iter().any(|p| p.pattern_type == "systematic_contamination");
        assert!(has_sys, "should detect systematic contamination");
    }

    #[test]
    fn test_extract_invariants() {
        let history = vec![
            make_metric("filengine_fs_read", 1, 0.8, 0.95, 0.75),
            make_metric("filengine_fs_read", 2, 0.8, 0.93, 0.75),
            make_metric("filengine_fs_read", 3, 0.8, 0.94, 0.75),
            make_metric("filengine_fs_read", 4, 0.8, 0.95, 0.75),
        ];
        let inv = extract_invariants(&history);
        let has_high = inv.iter().any(|i| i.contains("success_high"));
        assert!(has_high, "should detect high success rate");
        let has_vol = inv.iter().any(|i| i.contains("adoption_stable"));
        assert!(has_vol, "should detect stable adoption");
    }

    #[test]
    fn test_linear_trend() {
        let vals = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let slope = linear_trend(&vals);
        assert!((slope - 1.0).abs() < 0.01, "slope should be ~1.0, got {slope}");

        let flat = vec![3.0, 3.0, 3.0, 3.0];
        let flat_slope = linear_trend(&flat);
        assert!(flat_slope.abs() < 0.01, "flat slope should be ~0");
    }
}
