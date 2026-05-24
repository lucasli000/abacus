//! WorkflowCheckers — 15 阶段审查器实现
//!
//! ## 设计原则
//! - 规则引擎优先（<5ms/阶段），LLM 调用仅在规则无法判定时使用
//! - 领域自适应：通过 WorkflowContext.task_kind 路由到不同审查维度
//! - 无状态：审查器本身不持有状态，所有信息从 WorkflowContext 读取
//!
//! ## 依赖
//! - `workflow_gate`: PhaseChecker, PhaseVerdict, WorkflowContext, ReviewDimensions
//! - `preflight`: PreflightChecker (Phase 1 复用)
//! - `inertia`: InertiaDetector 的检测维度 (Phase 11 复用)
//! - `humanizer`: AIPatternDetector (Phase 14 复用)
//!
//! ## 引用关系
//! - 被 `WorkflowEngine::register_checker()` 注册
//! - 在 `WorkflowEngine::drive()` 中按阶段调用

use crate::core::workflow_gate::*;
use crate::core::task_analyzer::TaskKind;

// ─── Phase 1: IntentVerifier（需求审题）────────────────────────────────────

/// 确认理解用户意图：检测歧义、隐含需求、缺失信息
pub struct IntentVerifier;

#[async_trait::async_trait]
impl PhaseChecker for IntentVerifier {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let input = &ctx.input;
        let mut findings = Vec::new();

        // 歧义检测：过短/过模糊
        if input.chars().count() < 10 {
            findings.push("输入过短，可能缺少上下文".into());
        }

        // 检测常见歧义模式
        let ambiguous_patterns = ["这个", "那个", "它", "上面的", "之前的"];
        let has_reference = ambiguous_patterns.iter().any(|p| input.contains(p));
        if has_reference && input.chars().count() < 50 {
            findings.push("包含指代词但缺少明确上下文".into());
        }

        // 多重目标检测（"并且"/"同时"/"还要"拆分为多个子任务？）
        let multi_goal_markers = ["并且", "同时", "还要", "另外", "and also", "additionally"];
        let multi_count = multi_goal_markers.iter().filter(|m| input.contains(*m)).count();
        if multi_count >= 2 {
            findings.push("检测到多重目标，建议拆分为独立子任务".into());
        }

        // 破坏性操作检测（提前标记）
        let destructive = ["删除", "覆盖", "替换全部", "drop", "truncate", "rm -rf", "reset --hard"];
        if destructive.iter().any(|d| input.to_lowercase().contains(d)) {
            findings.push("包含破坏性操作，需要确认范围和回退方案".into());
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "comprehension" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::Comprehension }
}

// ─── Phase 2: RequirementDecomposer（需求梳理）──────────────────────────────

/// 拆解为结构化子需求
pub struct RequirementDecomposer;

#[async_trait::async_trait]
impl PhaseChecker for RequirementDecomposer {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();

        // 检查是否有足够的需求信息进行拆解
        let input_len = ctx.input.chars().count();
        if input_len < 20 && ctx.confirmed_intent.is_none() {
            findings.push("需求信息不足，难以有效拆解".into());
        }

        // 按领域提供拆解建议
        let decompose_hint = match ctx.task_kind {
            TaskKind::CodeWriting => "建议拆解为：接口设计 → 核心逻辑 → 错误处理 → 测试",
            TaskKind::Architecture => "建议拆解为：需求约束 → 候选架构 → 评估标准 → 选型",
            TaskKind::DataAnalysis => "建议拆解为：数据源 → 清洗规则 → 分析方法 → 输出格式",
            TaskKind::Debugging => "建议拆解为：复现条件 → 最小化 → 根因定位 → 修复验证",
            TaskKind::Linguistics => "建议拆解为：目标受众 → 核心信息 → 结构大纲 → 风格基调",
            TaskKind::Review => "建议拆解为：审查范围 → 审查维度 → 评级标准 → 输出格式",
            _ => "建议拆解为：目标 → 约束 → 步骤 → 验收标准",
        };
        findings.push(decompose_hint.into());

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "decomposition" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::Decomposition }
}

// ─── Phase 3: SolutionGenerator（需求方案）──────────────────────────────────

/// 检查是否生成了足够的候选方案
pub struct SolutionGenerator;

#[async_trait::async_trait]
impl PhaseChecker for SolutionGenerator {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();

        if ctx.candidate_solutions.is_empty() {
            findings.push("尚未生成候选方案".into());
        } else if ctx.candidate_solutions.len() < 2 && ctx.complexity_score > 0.5 {
            findings.push("复杂任务建议至少 2 个候选方案进行对比".into());
        }

        // 检查方案是否有明确的 pros/cons
        for sol in &ctx.candidate_solutions {
            if sol.pros.is_empty() && sol.cons.is_empty() {
                findings.push(format!("方案 '{}' 缺少利弊分析", sol.id));
            }
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "solution_design" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::SolutionDesign }
}

// ─── Phase 4: ConsistencyChecker（方案内部审查）─────────────────────────────

/// 单方案内部一致性/完整性/可行性
pub struct ConsistencyChecker;

#[async_trait::async_trait]
impl PhaseChecker for ConsistencyChecker {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();

        let solution = match ctx.selected_solution() {
            Some(s) => s,
            None => {
                // 没有选中方案，检查是否有候选
                if ctx.candidate_solutions.is_empty() {
                    return PhaseVerdict::Rollback {
                        to_phase: "solution_design".into(),
                        reason: "无候选方案，需要先设计方案".into(),
                    };
                }
                &ctx.candidate_solutions[0]
            }
        };

        // 一致性检查：pros 和 cons 是否自相矛盾
        for pro in &solution.pros {
            for con in &solution.cons {
                let pro_lower = pro.to_lowercase();
                let con_lower = con.to_lowercase();
                // 简单重叠检测
                let pro_words: std::collections::HashSet<&str> = pro_lower.split_whitespace().collect();
                let con_words: std::collections::HashSet<&str> = con_lower.split_whitespace().collect();
                let overlap = pro_words.intersection(&con_words).count();
                if overlap > 3 {
                    findings.push(format!(
                        "pros/cons 可能矛盾: '{}' vs '{}'", pro, con
                    ));
                }
            }
        }

        // 完整性检查：方案描述是否足够
        if solution.description.chars().count() < 20 {
            findings.push("方案描述过于简略，缺少实施细节".into());
        }
        if solution.approach.is_empty() {
            findings.push("缺少具体方法/途径描述".into());
        }

        // 检查是否有关键发现需要回退
        let critical_count = findings.iter()
            .filter(|f| f.contains("矛盾"))
            .count();
        if critical_count >= 2 {
            return PhaseVerdict::Rollback {
                to_phase: "solution_design".into(),
                reason: "方案内部存在多处矛盾，需要重新设计".into(),
            };
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "internal_review" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::InternalReview }
}

// ─── Phase 5: ArchitectureChecker（方案关联审查）────────────────────────────

/// 与现有代码/架构/约定的兼容性检查
pub struct ArchitectureChecker;

#[async_trait::async_trait]
impl PhaseChecker for ArchitectureChecker {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();

        // 基于领域的关联检查建议
        match ctx.task_kind {
            TaskKind::CodeWriting | TaskKind::FileEdit => {
                findings.push("检查：新代码是否与现有模块的接口约定一致".into());
                findings.push("检查：命名规范是否与 codebase 一致".into());
                findings.push("检查：错误处理模式是否与现有 pattern 一致".into());
            }
            TaskKind::Architecture => {
                findings.push("检查：新架构与现有系统的耦合点".into());
                findings.push("检查：数据流是否与现有 pipeline 兼容".into());
                findings.push("检查：部署方式是否与现有 infra 一致".into());
            }
            TaskKind::DataAnalysis => {
                findings.push("检查：数据源格式与现有 schema 兼容性".into());
                findings.push("检查：输出格式是否符合下游消费方期望".into());
            }
            TaskKind::Linguistics | TaskKind::Review => {
                findings.push("检查：术语使用是否与现有文档一致".into());
                findings.push("检查：格式/结构是否符合已有模板".into());
            }
            _ => {
                findings.push("检查：输出与现有约定/期望的一致性".into());
            }
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "cross_reference_review" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::CrossReferenceReview }
}

// ─── Phase 6: ImpactAnalyzer（连环影响审查）─────────────────────────────────

/// 变更波及范围分析
pub struct ImpactAnalyzer;

#[async_trait::async_trait]
impl PhaseChecker for ImpactAnalyzer {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();

        // 高影响范围检测
        let cascade_count = ctx.cascade_affected.len();
        if cascade_count > 10 {
            return PhaseVerdict::Rollback {
                to_phase: "solution_design".into(),
                reason: format!("影响范围过大: {} 个关联项受波及，建议缩小方案范围", cascade_count),
            };
        } else if cascade_count > 5 {
            findings.push(format!("中等影响范围: {} 个关联项需要关注", cascade_count));
        }

        // 基于领域的影响评估
        match ctx.task_kind {
            TaskKind::CodeWriting | TaskKind::FileEdit => {
                findings.push("评估：依赖此模块的消费方是否需要适配".into());
                findings.push("评估：是否需要 migration/版本兼容处理".into());
            }
            TaskKind::Architecture => {
                findings.push("评估：架构变更是否需要跨团队协调".into());
                findings.push("评估：是否影响 SLA/可用性承诺".into());
            }
            TaskKind::DataAnalysis => {
                findings.push("评估：数据变更是否影响已有报表/仪表板".into());
            }
            _ => {
                findings.push("评估：变更对关联方的影响".into());
            }
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "cascade_impact" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::CascadeImpact }
}

// ─── Phase 7: RoiCalculator（ROI 评估）──────────────────────────────────────

/// 收益 vs 成本 vs 风险三角评估
pub struct RoiCalculator;

#[async_trait::async_trait]
impl PhaseChecker for RoiCalculator {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();

        if let Some(ref roi) = ctx.roi_verdict {
            if !roi.approved {
                return PhaseVerdict::Rollback {
                    to_phase: "solution_design".into(),
                    reason: format!("ROI 不通过 (ratio={:.2}): {}", roi.ratio, roi.rationale),
                };
            }
            findings.push(format!(
                "ROI 通过: benefit={:.1}, cost={:.1}, risk={:.1}, ratio={:.2}",
                roi.benefit_score, roi.cost_score, roi.risk_score, roi.ratio
            ));
        } else {
            // 无 ROI 数据时根据复杂度估算
            let effort_hint = if ctx.complexity_score > 0.7 {
                "高复杂度任务：确认投入产出比合理"
            } else {
                "中等复杂度：默认通过"
            };
            findings.push(effort_hint.into());
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "roi_evaluation" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::RoiEvaluation }
}

// ─── Phase 8: FinalSolutionGate（方案终版）──────────────────────────────────

/// 确认最终方案（用户审批点）
pub struct FinalSolutionGate;

#[async_trait::async_trait]
impl PhaseChecker for FinalSolutionGate {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();

        if ctx.approved_solution.is_none() && ctx.candidate_solutions.is_empty() {
            return PhaseVerdict::Rollback {
                to_phase: "solution_design".into(),
                reason: "无可用方案进入终版阶段".into(),
            };
        }

        // 汇总前序审查的关键发现
        let critical_findings: Vec<&ReviewFinding> = ctx.internal_findings.iter()
            .chain(ctx.cross_ref_findings.iter())
            .filter(|f| f.severity == FindingSeverity::Critical)
            .collect();

        if !critical_findings.is_empty() {
            return PhaseVerdict::Rollback {
                to_phase: "solution_design".into(),
                reason: format!("{} 个 Critical 级别问题未解决", critical_findings.len()),
            };
        }

        findings.push("方案已通过前序审查，可进入执行阶段".into());
        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "final_solution" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::FinalSolution }
}

// ─── Phase 9: StepPlanner（执行构思）────────────────────────────────────────

/// 拆解为可验证的执行步骤
pub struct StepPlanner;

#[async_trait::async_trait]
impl PhaseChecker for StepPlanner {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();

        if ctx.execution_steps.is_empty() {
            findings.push("执行步骤未规划，建议至少拆解为 2-3 个可验证步骤".into());
        } else {
            findings.push(format!("已规划 {} 个执行步骤", ctx.execution_steps.len()));
        }

        if ctx.rollback_plan.is_none() && ctx.complexity_score > 0.5 {
            findings.push("复杂任务缺少回退方案，建议补充".into());
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "execution_planning" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::ExecutionPlanning }
}

// ─── Phase 10: ExecutionGate（执行）─────────────────────────────────────────

/// 执行阶段门控（由 WorkflowEngine 在此处调用 TurnPipeline）
pub struct ExecutionGate;

#[async_trait::async_trait]
impl PhaseChecker for ExecutionGate {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        // Phase 10 的实际执行由 WorkflowEngine 外部处理（调用 TurnPipeline）
        // 这里只做后置验证：执行结果是否有效
        let mut findings = Vec::new();

        if let Some(ref output) = ctx.execution_output {
            if output.is_empty() {
                findings.push("执行产出为空".into());
            } else if output.chars().count() < 20 {
                findings.push("执行产出过短，可能不完整".into());
            }
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "execution" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::Execution }
}

// ─── Phase 11: MultiReviewer（多视角审查）────────────────────────────────────

/// 按领域动态选择 4 维度审查
pub struct MultiReviewer;

#[async_trait::async_trait]
impl PhaseChecker for MultiReviewer {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();

        let dims = ReviewDimensions::for_multi_perspective(&ctx.task_kind);
        let output = ctx.execution_output.as_deref().unwrap_or("");

        for dim in &dims.dimensions {
            // 规则引擎快速评估（每个维度一个简单检查）
            let score = rule_based_dimension_check(output, &dim.name, &ctx.task_kind);
            if score < dim.threshold {
                findings.push(format!(
                    "[{}] 未达标 ({:.0}% < {:.0}%): {}",
                    dim.name, score * 100.0, dim.threshold * 100.0, dim.description
                ));
            }
        }

        // 多个维度不达标 → 需要回退修复
        let fail_count = findings.iter().filter(|f| f.contains("未达标")).count();
        if fail_count >= 3 {
            return PhaseVerdict::Rollback {
                to_phase: "execution".into(),
                reason: format!("{} 个审查维度未通过，需要修复后重新执行", fail_count),
            };
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "multi_perspective_review" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::MultiPerspectiveReview }
}

// ─── Phase 12: LayerReviewer（渐进式分层审查）───────────────────────────────

/// 表层→结构→语义→系统 层层递进
pub struct LayerReviewer;

#[async_trait::async_trait]
impl PhaseChecker for LayerReviewer {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();
        let output = ctx.execution_output.as_deref().unwrap_or("");

        let dims = ReviewDimensions::for_progressive_layer(&ctx.task_kind);
        for (i, dim) in dims.dimensions.iter().enumerate() {
            // 层层递进：前一层通过才检查下一层
            let layer_score = rule_based_layer_check(output, i, &ctx.task_kind);
            if layer_score < dim.threshold {
                findings.push(format!(
                    "[Layer {}] {}: 在此层发现问题，后续层检查终止",
                    i + 1, dim.description
                ));
                break; // 递进式：当前层失败则不继续
            } else {
                findings.push(format!("[Layer {}] {}: 通过", i + 1, dim.name));
            }
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "progressive_layer_review" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::ProgressiveLayerReview }
}

// ─── Phase 13: StressTester（对抗性压力测试）─────────────────────────────────

/// 模拟恶意输入/边界条件/故障注入
pub struct StressTester;

#[async_trait::async_trait]
impl PhaseChecker for StressTester {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();
        let output = ctx.execution_output.as_deref().unwrap_or("");

        let dims = ReviewDimensions::for_adversarial(&ctx.task_kind);
        for dim in &dims.dimensions {
            let issues = adversarial_check(output, &dim.name, &ctx.task_kind);
            if !issues.is_empty() {
                for issue in &issues {
                    findings.push(format!("[{}] {}", dim.name, issue));
                }
            }
        }

        // Critical 级别的对抗性问题 → 回退
        let critical_count = findings.iter()
            .filter(|f| f.contains("injection") || f.contains("注入"))
            .count();
        if critical_count > 0 {
            return PhaseVerdict::Rollback {
                to_phase: "execution".into(),
                reason: format!("发现 {} 个安全性问题，需要修复", critical_count),
            };
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "adversarial_stress" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::AdversarialStress }
}

// ─── Phase 14: PerceptionChecker（用户感知验证）─────────────────────────────

/// 输出是否对用户有价值 + 可理解
pub struct PerceptionChecker;

#[async_trait::async_trait]
impl PhaseChecker for PerceptionChecker {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();
        let output = ctx.execution_output.as_deref().unwrap_or("");

        // 清晰度检查
        let avg_sentence_len = if output.is_empty() { 0 } else {
            let sentences = output.matches(|c: char| "。！？.!?".contains(c)).count().max(1);
            output.chars().count() / sentences
        };
        if avg_sentence_len > 80 {
            findings.push("平均句子过长，建议拆分以提升可读性".into());
        }

        // 行动性检查：输出是否有明确的下一步
        let has_action = output.contains("建议") || output.contains("步骤")
            || output.contains("需要") || output.contains("should")
            || output.contains("recommend") || output.contains("next step");
        if !has_action && ctx.task_kind != TaskKind::KnowledgeQuery {
            findings.push("输出缺少明确的行动建议或下一步指引".into());
        }

        // AI 味检测（复用 humanizer 逻辑的简化版）
        let ai_words = ["至关重要", "不容忽视", "深入探讨", "综上所述", "leverage", "pivotal"];
        let ai_hits = ai_words.iter().filter(|w| output.contains(*w)).count();
        if ai_hits >= 3 {
            findings.push(format!("检测到 {} 个 AI 味高频词，建议精简表达", ai_hits));
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "user_perception_check" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::UserPerceptionCheck }
}

// ─── Phase 15: MaintenanceAssessor（长期维护评估）───────────────────────────

/// 可维护性/可扩展性/技术债务评估
pub struct MaintenanceAssessor;

#[async_trait::async_trait]
impl PhaseChecker for MaintenanceAssessor {
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict {
        let mut findings = Vec::new();
        let output = ctx.execution_output.as_deref().unwrap_or("");

        match ctx.task_kind {
            TaskKind::CodeWriting | TaskKind::FileEdit | TaskKind::Architecture => {
                // 代码可维护性检查
                if output.contains("TODO") || output.contains("FIXME") || output.contains("HACK") {
                    findings.push("输出中包含 TODO/FIXME/HACK 标记，存在技术债务".into());
                }
                if output.contains("unwrap()") && !output.contains("test") {
                    findings.push("非测试代码中使用 unwrap() 可能导致 panic".into());
                }
                // magic number 检测
                let has_magic = output.lines().any(|line| {
                    let trimmed = line.trim();
                    !trimmed.starts_with("//") && !trimmed.starts_with("#")
                        && trimmed.contains(|c: char| c.is_ascii_digit())
                        && (trimmed.contains("== ") || trimmed.contains("> ") || trimmed.contains("< "))
                        && !trimmed.contains("0") && !trimmed.contains("1")
                });
                if has_magic {
                    findings.push("检测到可能的 magic number，建议提取为常量".into());
                }
            }
            TaskKind::DataAnalysis => {
                findings.push("评估：数据管道是否可重复执行（幂等性）".into());
                findings.push("评估：是否有监控/告警覆盖".into());
            }
            TaskKind::Linguistics | TaskKind::Review => {
                findings.push("评估：文档是否有版本管理/更新机制".into());
            }
            _ => {
                findings.push("评估：输出是否可在未来场景中复用".into());
            }
        }

        PhaseVerdict::Pass { findings }
    }

    fn phase_name(&self) -> &'static str { "maintenance_assessment" }
    fn target_phase(&self) -> WorkflowPhase { WorkflowPhase::MaintenanceAssessment }
}

// ─── 规则引擎辅助函数 ────────────────────────────────────────────────────

/// 基于规则的维度评分（Phase 11 用）
fn rule_based_dimension_check(output: &str, dim_name: &str, task_kind: &TaskKind) -> f64 {
    if output.is_empty() { return 0.3; }

    match (dim_name, task_kind) {
        // 代码安全性：检测危险模式
        ("security", TaskKind::CodeWriting | TaskKind::FileEdit) => {
            let dangers = ["eval(", "exec(", "system(", "unsafe {", "shell_exec",
                          "innerHTML", "dangerouslySetInnerHTML"];
            let hits = dangers.iter().filter(|d| output.contains(*d)).count();
            if hits > 0 { 0.3 } else { 0.9 }
        }
        // 代码正确性：基础结构检查
        ("correctness", TaskKind::CodeWriting | TaskKind::FileEdit) => {
            let has_error_handling = output.contains("Result") || output.contains("Error")
                || output.contains("?") || output.contains("try") || output.contains("catch");
            if has_error_handling { 0.8 } else { 0.6 }
        }
        // 数据质量
        ("data_quality", TaskKind::DataAnalysis) => {
            let has_validation = output.contains("validate") || output.contains("check")
                || output.contains("assert") || output.contains("verify");
            if has_validation { 0.8 } else { 0.5 }
        }
        // 逻辑完备
        ("logical_completeness", TaskKind::Mathematics) => {
            let has_proof = output.contains("因此") || output.contains("所以")
                || output.contains("therefore") || output.contains("QED")
                || output.contains("证明");
            if has_proof { 0.8 } else { 0.5 }
        }
        // 事实准确（文案场景）
        ("factual_accuracy", TaskKind::Linguistics) => {
            // 无法完全通过规则验证事实，给中性分
            0.7
        }
        // 默认：给通过分
        _ => 0.75,
    }
}

/// 基于规则的分层检查（Phase 12 用）
fn rule_based_layer_check(output: &str, layer: usize, _task_kind: &TaskKind) -> f64 {
    if output.is_empty() { return 0.3; }

    match layer {
        0 => {
            // 表层：格式/拼写基础检查
            let has_structure = output.contains('\n') && output.lines().count() > 1;
            if has_structure { 0.9 } else { 0.6 }
        }
        1 => {
            // 结构：组织合理性
            let has_sections = output.contains("##") || output.contains("###")
                || output.lines().any(|l| l.starts_with("- ") || l.starts_with("1."));
            if has_sections { 0.8 } else { 0.6 }
        }
        2 => {
            // 语义：逻辑连贯性（简化检测）
            let sentences = output.matches(|c: char| "。.!?！？".contains(c)).count();
            if sentences >= 2 { 0.8 } else { 0.5 }
        }
        3 => {
            // 系统：整体一致性
            0.75 // 规则引擎难以判断系统级一致性，给中性分
        }
        _ => 0.7,
    }
}

/// 对抗性检查（Phase 13 用）
fn adversarial_check(output: &str, dim_name: &str, task_kind: &TaskKind) -> Vec<String> {
    let mut issues = Vec::new();

    match (dim_name, task_kind) {
        ("injection", TaskKind::CodeWriting | TaskKind::FileEdit) => {
            // SQL 注入模式
            if output.contains("format!(") && output.contains("SELECT") {
                issues.push("潜在 SQL 注入：使用 format! 拼接 SQL".into());
            }
            // 命令注入
            if output.contains("Command::new") && output.contains("format!(") {
                issues.push("潜在命令注入：format! 拼接命令参数".into());
            }
        }
        ("edge_cases", _)
            // 检查是否处理了空/null
            if output.contains("fn ") && !output.contains("empty") && !output.contains("None")
                && !output.contains("is_empty") && output.lines().count() > 10
            => {
                issues.push("函数可能未处理空值/空集合场景".into());
            }
        ("dirty_data", TaskKind::DataAnalysis)
            if !output.contains("NaN") && !output.contains("null") && !output.contains("missing") => {
                issues.push("数据处理未提及缺失值/异常值处理".into());
            }
        ("factual_challenge", TaskKind::Linguistics | TaskKind::Review) => {
            // 检测未标注来源的断言
            let assertion_words = ["根据", "据统计", "研究表明", "数据显示"];
            let has_assertion = assertion_words.iter().any(|w| output.contains(w));
            let has_source = output.contains("来源") || output.contains("参考")
                || output.contains("http") || output.contains("Source");
            if has_assertion && !has_source {
                issues.push("包含事实断言但未标注来源".into());
            }
        }
        _ => {}
    }

    issues
}

// ─── 注册所有审查器 ──────────────────────────────────────────────────────

/// 将所有 15 个审查器注册到 WorkflowEngine
pub fn register_all_checkers(engine: &mut super::workflow_engine::WorkflowEngine) {
    use std::sync::Arc;
    engine.register_checker(Arc::new(IntentVerifier));
    engine.register_checker(Arc::new(RequirementDecomposer));
    engine.register_checker(Arc::new(SolutionGenerator));
    engine.register_checker(Arc::new(ConsistencyChecker));
    engine.register_checker(Arc::new(ArchitectureChecker));
    engine.register_checker(Arc::new(ImpactAnalyzer));
    engine.register_checker(Arc::new(RoiCalculator));
    engine.register_checker(Arc::new(FinalSolutionGate));
    engine.register_checker(Arc::new(StepPlanner));
    engine.register_checker(Arc::new(ExecutionGate));
    engine.register_checker(Arc::new(MultiReviewer));
    engine.register_checker(Arc::new(LayerReviewer));
    engine.register_checker(Arc::new(StressTester));
    engine.register_checker(Arc::new(PerceptionChecker));
    engine.register_checker(Arc::new(MaintenanceAssessor));
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx(task_kind: TaskKind, input: &str) -> WorkflowContext {
        WorkflowContext::new(
            input.into(), task_kind, 0.7, WorkflowMode::Full,
        )
    }

    #[tokio::test]
    async fn test_intent_verifier_short_input() {
        let ctx = make_ctx(TaskKind::CodeWriting, "fix");
        let v = IntentVerifier.check(&ctx).await;
        match v {
            PhaseVerdict::Pass { findings } => {
                assert!(findings.iter().any(|f| f.contains("过短")));
            }
            _ => panic!("expected Pass"),
        }
    }

    #[tokio::test]
    async fn test_intent_verifier_destructive() {
        let ctx = make_ctx(TaskKind::FileEdit, "删除所有日志文件");
        let v = IntentVerifier.check(&ctx).await;
        match v {
            PhaseVerdict::Pass { findings } => {
                assert!(findings.iter().any(|f| f.contains("破坏性")));
            }
            _ => panic!("expected Pass"),
        }
    }

    #[tokio::test]
    async fn test_consistency_checker_no_solution() {
        let ctx = make_ctx(TaskKind::Architecture, "design new system");
        let v = ConsistencyChecker.check(&ctx).await;
        assert!(v.is_rollback());
    }

    #[tokio::test]
    async fn test_multi_reviewer_code() {
        let mut ctx = make_ctx(TaskKind::CodeWriting, "write parser");
        ctx.execution_output = Some("fn parse(input: &str) -> Result<AST, Error> {\n    Ok(AST::new())\n}".into());
        let v = MultiReviewer.check(&ctx).await;
        assert!(v.is_pass());
    }

    #[tokio::test]
    async fn test_stress_tester_injection() {
        let mut ctx = make_ctx(TaskKind::CodeWriting, "query db");
        ctx.execution_output = Some(r#"
            let query = format!("SELECT * FROM users WHERE name = '{}'", user_input);
            Command::new("sh").arg(format!("-c {}", cmd)).output();
        "#.into());
        let v = StressTester.check(&ctx).await;
        assert!(v.is_rollback(), "should rollback on injection pattern");
    }

    #[tokio::test]
    async fn test_perception_checker_ai_words() {
        let mut ctx = make_ctx(TaskKind::Linguistics, "write article");
        ctx.execution_output = Some(
            "在当今数字时代，数据安全至关重要。不容忽视的是，深入探讨加密方案的意义。综上所述，这是关键。".into()
        );
        let v = PerceptionChecker.check(&ctx).await;
        match v {
            PhaseVerdict::Pass { findings } => {
                assert!(findings.iter().any(|f| f.contains("AI 味")));
            }
            _ => panic!("expected Pass with AI finding"),
        }
    }

    #[tokio::test]
    async fn test_maintenance_assessor_todo() {
        let mut ctx = make_ctx(TaskKind::CodeWriting, "impl feature");
        ctx.execution_output = Some("fn handle() {\n    // TODO: implement\n    unimplemented!()\n}".into());
        let v = MaintenanceAssessor.check(&ctx).await;
        match v {
            PhaseVerdict::Pass { findings } => {
                assert!(findings.iter().any(|f| f.contains("TODO")));
            }
            _ => panic!("expected Pass"),
        }
    }
}
