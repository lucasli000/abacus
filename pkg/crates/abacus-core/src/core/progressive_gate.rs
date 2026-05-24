//! 渐进输出门控决策器
//!
//! ## 场景
//! 每次 LLM 请求前，评估是否需要启动渐进输出协议。
//!
//! ## 依赖
//! - `abacus_types::progressive::*`: 类型定义
//! - `crate::core::task_analyzer::TaskAnalyzer`: 复杂度评分
//!
//! ## 引用关系
//! - 被 `ProgressiveController::begin()` 调用
//! - 被 `CoreLoop` 在 pre-LLM 阶段间接调用
//!
//! ## 边界
//! - Gate 判断是纯函数（无副作用），可反复调用
//! - ThresholdCalibrator 有内部状态（EMA 历史）

use crate::config::ConfigManager;
use abacus_types::progressive::*;

/// 门控配置（从 ConfigManager 读取）
///
/// ## 场景
/// 系统启动时从 Setting.yaml 加载，运行时不可变（除 calibrator 微调）。
#[derive(Debug, Clone)]
pub struct GateConfig {
    pub enabled: bool,
    pub threshold_passthrough: f64,
    pub threshold_gated: f64,
    pub forced_gated_types: Vec<String>,
    pub forced_passthrough: Vec<String>,
    pub checklist_timeout_secs: u64,
    pub max_checklist_items: u32,
    pub team_exempt_in_execution: bool,
    /// ThresholdCalibrator 的 EMA 学习率
    pub calibrator_alpha: f64,
    /// ThresholdCalibrator 的漂移限制
    pub calibrator_drift_limit: f64,
    /// ThresholdCalibrator 的 passthrough/gated 最小间距
    pub calibrator_min_gap: f64,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_passthrough: 0.30,
            threshold_gated: 0.70,
            forced_gated_types: vec![
                "prd".into(), "sop".into(), "architecture_design".into(),
                "financial_report".into(), "compliance_doc".into(),
            ],
            forced_passthrough: vec![
                "simple_query".into(), "code_snippet".into(),
                "translation".into(), "greeting".into(),
            ],
            checklist_timeout_secs: 300,
            max_checklist_items: 7,
            team_exempt_in_execution: true,
            calibrator_alpha: 0.10,
            calibrator_drift_limit: 0.10,
            calibrator_min_gap: 0.15,
        }
    }
}

/// 门控决策器
///
/// ## 场景
/// 给定任务分析结果 + 用户配置的自主程度 + 作用域，返回输出策略。
///
/// ## 边界
/// - disabled 时永远返回 PassThrough
/// - scope 为 TeamInExecution 时永远返回 PassThrough
pub struct ProgressiveGate {
    config: GateConfig,
}

impl ProgressiveGate {
    pub fn new(config: GateConfig) -> Self {
        Self { config }
    }

    /// 从 AutonomyLevel 构建（便捷入口）
    pub fn from_autonomy(level: AutonomyLevel) -> Self {
        let (pt, gt) = level.to_thresholds();
        Self {
            config: GateConfig {
                threshold_passthrough: pt,
                threshold_gated: gt,
                forced_gated_types: level.forced_gated_types()
                    .into_iter().map(String::from).collect(),
                ..Default::default()
            },
        }
    }

    /// 从 ConfigManager 读取配置构建（R1: 修复配置孤岛）
    ///
    /// ## 加载的键
    /// - progressive.enabled
    /// - progressive.autonomy_level
    /// - progressive.threshold_passthrough / threshold_gated
    /// - progressive.forced_gated_types
    /// - progressive.checklist_timeout_secs / max_checklist_items
    /// - progressive.team_exempt_in_execution
    /// - progressive.calibrator_alpha / calibrator_drift_limit / calibrator_min_gap
    pub fn from_config_manager(mgr: &ConfigManager) -> Self {
        let autonomy_str = mgr.get_str("progressive.autonomy_level").unwrap_or("medium");
        let autonomy = match autonomy_str {
            "full" => AutonomyLevel::Full,
            "high" => AutonomyLevel::High,
            "low" => AutonomyLevel::Low,
            _ => AutonomyLevel::Medium,
        };

        let forced_list: Vec<String> = if let Some(v) = mgr.get("progressive.forced_gated_types") {
            if let crate::config::ConfigValue::List(items) = &v.value {
                items.iter().filter_map(|i| i.as_str().map(String::from)).collect()
            } else { autonomy.forced_gated_types().into_iter().map(String::from).collect() }
        } else { autonomy.forced_gated_types().into_iter().map(String::from).collect() };

        let enabled = mgr.get_bool("progressive.enabled").unwrap_or(true);
        let threshold_passthrough = mgr.get_number("progressive.threshold_passthrough")
            .map(|n| n.clamp(0.0, 1.0))
            .unwrap_or_else(|| autonomy.to_thresholds().0);
        let threshold_gated = mgr.get_number("progressive.threshold_gated")
            .map(|n| n.clamp(0.0, 1.0))
            .unwrap_or_else(|| autonomy.to_thresholds().1);

        Self {
            config: GateConfig {
                enabled,
                threshold_passthrough,
                threshold_gated,
                forced_gated_types: forced_list,
                forced_passthrough: vec![
                    "simple_query".into(), "code_snippet".into(),
                    "translation".into(), "greeting".into(),
                ],
                checklist_timeout_secs: mgr.get_number("progressive.checklist_timeout_secs")
                    .map(|n| n as u64).unwrap_or(300),
                max_checklist_items: mgr.get_number("progressive.max_checklist_items")
                    .map(|n| n as u32).unwrap_or(7),
                team_exempt_in_execution: mgr.get_bool("progressive.team_exempt_in_execution")
                    .unwrap_or(true),
                calibrator_alpha: mgr.get_number("progressive.calibrator_alpha")
                    .map(|n| n.clamp(0.01, 0.50)).unwrap_or(0.10),
                calibrator_drift_limit: mgr.get_number("progressive.calibrator_drift_limit")
                    .map(|n| n.clamp(0.01, 0.30)).unwrap_or(0.10),
                calibrator_min_gap: mgr.get_number("progressive.calibrator_min_gap")
                    .map(|n| n.clamp(0.05, 0.40)).unwrap_or(0.15),
            },
        }
    }

    /// 核心决策：返回输出策略
    ///
    /// ## 决策优先级
    /// 1. disabled → PassThrough
    /// 2. TeamInExecution → PassThrough
    /// 3. 强制直通类型 → PassThrough
    /// 4. 强制门控类型 → Gated
    /// 5. Low + has_decisions → Gated（跳过 Staged）
    /// 6. 阈值判断
    pub fn decide(
        &self,
        profile: &ComplexityProfile,
        autonomy: AutonomyLevel,
        scope: GateScope,
    ) -> OutputStrategy {
        // 全局开关
        if !self.config.enabled {
            return OutputStrategy::PassThrough;
        }

        // Team 执行中豁免
        if scope == GateScope::TeamInExecution && self.config.team_exempt_in_execution {
            return OutputStrategy::PassThrough;
        }

        // 强制直通
        // (注：这里用 task_type label 匹配，需要 caller 传入)
        if self.config.forced_passthrough.iter().any(|t| t == "general_chat") && profile.score < 0.1 {
            return OutputStrategy::PassThrough;
        }

        // 强制门控类型（按 autonomy level 配置）
        let forced = autonomy.forced_gated_types();
        // 此检查需要外部传入 task_type，暂用 profile 信号代替
        // 当 domain 命中 forced 列表中的关键词时触发
        // Note: forced_gated_types 在 decide_with_task_type() 中完整实现
        // 此简化版 decide() 仅用 complexity score 判断
        let _ = forced;

        // Low 模式特殊规则：有决策信号直接 Gate
        if autonomy == AutonomyLevel::Low && profile.has_decisions
            && profile.score >= self.config.threshold_passthrough {
                return OutputStrategy::Gated {
                    checklist: Checklist::placeholder(),
                };
            }

        // 阈值判断
        let score = profile.score;
        if score < self.config.threshold_passthrough {
            OutputStrategy::PassThrough
        } else if score < self.config.threshold_gated {
            OutputStrategy::Staged { sections: Vec::new() }
        } else {
            OutputStrategy::Gated {
                checklist: Checklist::placeholder(),
            }
        }
    }

    /// 带任务类型的决策（完整版）
    pub fn decide_with_task_type(
        &self,
        profile: &ComplexityProfile,
        autonomy: AutonomyLevel,
        scope: GateScope,
        task_type: &str,
    ) -> OutputStrategy {
        if !self.config.enabled {
            return OutputStrategy::PassThrough;
        }
        if scope == GateScope::TeamInExecution && self.config.team_exempt_in_execution {
            return OutputStrategy::PassThrough;
        }

        // 强制直通
        if self.config.forced_passthrough.contains(&task_type.to_string()) {
            return OutputStrategy::PassThrough;
        }

        // 强制门控
        let forced_types = autonomy.forced_gated_types();
        if forced_types.contains(&task_type) {
            return OutputStrategy::Gated {
                checklist: Checklist::placeholder(),
            };
        }

        // Low + decision → Gate
        if autonomy == AutonomyLevel::Low && profile.has_decisions
            && profile.score >= self.config.threshold_passthrough
        {
            return OutputStrategy::Gated {
                checklist: Checklist::placeholder(),
            };
        }

        // 阈值
        let score = profile.score;
        if score < self.config.threshold_passthrough {
            OutputStrategy::PassThrough
        } else if score < self.config.threshold_gated {
            OutputStrategy::Staged { sections: Vec::new() }
        } else {
            OutputStrategy::Gated {
                checklist: Checklist::placeholder(),
            }
        }
    }

    /// 执行后门控：基于偏差严重程度决定
    pub fn decide_post_execution(
        &self,
        deviations: &[Deviation],
        autonomy: AutonomyLevel,
    ) -> OutputStrategy {
        if deviations.is_empty() {
            return OutputStrategy::PassThrough;
        }

        let max_severity = deviations.iter()
            .map(|d| d.severity)
            .max()
            .unwrap_or(DeviationSeverity::Low);

        let should_gate = match autonomy {
            AutonomyLevel::Full => false,  // 全托管不门控
            AutonomyLevel::High => max_severity == DeviationSeverity::High,
            AutonomyLevel::Medium => max_severity >= DeviationSeverity::Medium,
            AutonomyLevel::Low => true,
        };

        if should_gate {
            OutputStrategy::Gated {
                checklist: Checklist::placeholder(),
            }
        } else {
            OutputStrategy::PassThrough
        }
    }

    pub fn config(&self) -> &GateConfig {
        &self.config
    }
}

// ─── ThresholdCalibrator ─────────────────────────────────────────────────

/// 阈值自适应校准器（EMA）
///
/// ## 场景
/// 通过用户行为反馈动态微调阈值。
/// skip_all → 阈值偏低 → 上调。
/// redo_request → 阈值偏高 → 下调。
///
/// ## 边界
/// - drift 不超过 ±drift_limit
/// - passthrough 与 gated 间距 ≥ min_gap
pub struct ThresholdCalibrator {
    pub passthrough: f64,
    pub gated: f64,
    alpha: f64,
    drift_limit: f64,
    min_gap: f64,
    base_passthrough: f64,
    base_gated: f64,
}

impl ThresholdCalibrator {
    pub fn new(passthrough: f64, gated: f64, alpha: f64, drift_limit: f64) -> Self {
        Self {
            passthrough,
            gated,
            alpha,
            drift_limit,
            min_gap: 0.15,
            base_passthrough: passthrough,
            base_gated: gated,
        }
    }

    /// 带 min_gap 参数的构造函数（R2: min_gap 可配置化）
    pub fn new_with_min_gap(passthrough: f64, gated: f64, alpha: f64, drift_limit: f64, min_gap: f64) -> Self {
        Self {
            passthrough,
            gated,
            alpha,
            drift_limit,
            min_gap,
            base_passthrough: passthrough,
            base_gated: gated,
        }
    }

    pub fn from_autonomy(level: AutonomyLevel, alpha: f64, drift_limit: f64) -> Self {
        let (pt, gt) = level.to_thresholds();
        Self::new(pt, gt, alpha, drift_limit)
    }

    /// 从 GateConfig 构建（R2: 使用可配置的 min_gap）
    pub fn from_gate_config(config: &GateConfig) -> Self {
        Self::new_with_min_gap(
            config.threshold_passthrough,
            config.threshold_gated,
            config.calibrator_alpha,
            config.calibrator_drift_limit,
            config.calibrator_min_gap,
        )
    }

    /// 用户跳过清单 → 门控阈值偏低，上调
    pub fn on_skipped(&mut self, score_at_trigger: f64) {
        self.gated += self.alpha * (score_at_trigger - self.gated + 0.1);
        self.clamp();
    }

    /// 用户要求重做 → 门控阈值偏高，下调
    pub fn on_redo_requested(&mut self, score_at_passthrough: f64) {
        self.gated -= self.alpha * (self.gated - score_at_passthrough + 0.1);
        self.clamp();
    }

    /// 正常确认 → 不调整
    pub fn on_normal_confirmation(&self) {
        // 正反馈不改阈值
    }

    pub fn thresholds(&self) -> (f64, f64) {
        (self.passthrough, self.gated)
    }

    fn clamp(&mut self) {
        // drift limit
        self.passthrough = self.passthrough
            .max(self.base_passthrough - self.drift_limit)
            .min(self.base_passthrough + self.drift_limit);
        self.gated = self.gated
            .max(self.base_gated - self.drift_limit)
            .min(self.base_gated + self.drift_limit);

        // min gap
        if self.gated - self.passthrough < self.min_gap {
            self.passthrough = self.gated - self.min_gap;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passthrough_when_disabled() {
        let config = GateConfig { enabled: false, ..Default::default() };
        let gate = ProgressiveGate::new(config);
        let profile = ComplexityProfile {
            score: 0.9,
            dimensions: ComplexityDimensions {
                input_length: 0.9, structural: 0.9, domain_crossing: 0.9,
                decision_density: 0.9, output_scale: 0.9,
                external_dependency: 0.9, precision_requirement: 0.9,
            },
            estimated_output_chars: 5000,
            has_decisions: true,
            needs_external_info: true,
            domain_count: 5,
            assessment_confidence: 1.0,
        };
        let result = gate.decide(&profile, AutonomyLevel::Low, GateScope::SingleAgent);
        assert_eq!(result, OutputStrategy::PassThrough);
    }

    #[test]
    fn test_team_in_execution_exempt() {
        let gate = ProgressiveGate::from_autonomy(AutonomyLevel::Low);
        let profile = ComplexityProfile {
            score: 0.9,
            dimensions: ComplexityDimensions {
                input_length: 0.9, structural: 0.9, domain_crossing: 0.9,
                decision_density: 0.9, output_scale: 0.9,
                external_dependency: 0.9, precision_requirement: 0.9,
            },
            estimated_output_chars: 5000,
            has_decisions: true,
            needs_external_info: true,
            domain_count: 5,
            assessment_confidence: 1.0,
        };
        let result = gate.decide(&profile, AutonomyLevel::Low, GateScope::TeamInExecution);
        assert_eq!(result, OutputStrategy::PassThrough);
    }

    #[test]
    fn test_high_autonomy_thresholds() {
        let gate = ProgressiveGate::from_autonomy(AutonomyLevel::High);
        let profile = ComplexityProfile {
            score: 0.75,
            dimensions: ComplexityDimensions {
                input_length: 0.5, structural: 0.5, domain_crossing: 0.5,
                decision_density: 0.5, output_scale: 0.5,
                external_dependency: 0.5, precision_requirement: 0.5,
            },
            estimated_output_chars: 3000,
            has_decisions: true,
            needs_external_info: false,
            domain_count: 3,
            assessment_confidence: 0.7,
        };
        // 0.75 is between High's PT(0.70) and Gated(0.90) → Staged
        let result = gate.decide(&profile, AutonomyLevel::High, GateScope::SingleAgent);
        assert!(matches!(result, OutputStrategy::Staged { .. }));
    }

    #[test]
    fn test_low_autonomy_decision_gate() {
        let gate = ProgressiveGate::from_autonomy(AutonomyLevel::Low);
        let profile = ComplexityProfile {
            score: 0.25, // above Low's PT(0.15)
            dimensions: ComplexityDimensions {
                input_length: 0.1, structural: 0.1, domain_crossing: 0.1,
                decision_density: 0.4, output_scale: 0.2,
                external_dependency: 0.0, precision_requirement: 0.0,
            },
            estimated_output_chars: 1000,
            has_decisions: true, // decision signal!
            needs_external_info: false,
            domain_count: 1,
            assessment_confidence: 0.3,
        };
        let result = gate.decide(&profile, AutonomyLevel::Low, GateScope::SingleAgent);
        // Low + has_decisions + score >= PT → Gated
        assert!(matches!(result, OutputStrategy::Gated { .. }));
    }

    #[test]
    fn test_post_execution_no_deviations() {
        let gate = ProgressiveGate::from_autonomy(AutonomyLevel::Medium);
        let result = gate.decide_post_execution(&[], AutonomyLevel::Medium);
        assert_eq!(result, OutputStrategy::PassThrough);
    }

    #[test]
    fn test_post_execution_high_deviation() {
        let gate = ProgressiveGate::from_autonomy(AutonomyLevel::High);
        let deviations = vec![Deviation {
            subtask_id: 1,
            expected: "x".into(),
            actual: "y".into(),
            severity: DeviationSeverity::High,
        }];
        let result = gate.decide_post_execution(&deviations, AutonomyLevel::High);
        assert!(matches!(result, OutputStrategy::Gated { .. }));
    }

    #[test]
    fn test_calibrator_drift_limit() {
        let mut cal = ThresholdCalibrator::from_autonomy(AutonomyLevel::Medium, 0.1, 0.10);
        // Repeatedly skip → should not exceed drift limit
        for _ in 0..20 {
            cal.on_skipped(0.9);
        }
        let (_, gated) = cal.thresholds();
        assert!(gated <= 0.80, "gated should not exceed base+drift, got {}", gated);
        assert!(gated >= 0.60, "gated should not go below base-drift, got {}", gated);
    }

    #[test]
    fn test_forced_gated_with_task_type() {
        let gate = ProgressiveGate::from_autonomy(AutonomyLevel::Medium);
        let profile = ComplexityProfile {
            score: 0.1, // very low
            dimensions: ComplexityDimensions {
                input_length: 0.0, structural: 0.0, domain_crossing: 0.0,
                decision_density: 0.0, output_scale: 0.0,
                external_dependency: 0.0, precision_requirement: 0.0,
            },
            estimated_output_chars: 200,
            has_decisions: false,
            needs_external_info: false,
            domain_count: 0,
            assessment_confidence: 0.1,
        };
        // "prd" is in Medium's forced list → should gate regardless of score
        let result = gate.decide_with_task_type(
            &profile, AutonomyLevel::Medium, GateScope::SingleAgent, "prd"
        );
        assert!(matches!(result, OutputStrategy::Gated { .. }));
    }
}
