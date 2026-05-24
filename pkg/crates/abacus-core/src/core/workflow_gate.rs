//! WorkflowGate — 全流程质量状态机
//!
//! ## 场景
//! 对复杂任务强制经历 15 阶段渐进式质量门控：
//!   Pre-Execution (1-8): 需求理解 → 方案定稿
//!   Execution (9-10): 执行构思 → 执行
//!   Post-Execution (11-15): 多视角渐进式审查
//!
//! 简单任务走 fast-path 跳过。
//!
//! ## 领域适配
//! 审查维度按 TaskKind 动态切换：
//! - Code → 安全/性能/正确性/可读性
//! - Content → 事实准确/语气/受众/完整性
//! - Data → 质量/一致性/时效性/覆盖度
//! - Math → 逻辑/边界/数值稳定性
//! - Architecture → 扩展性/耦合/一致性/运维
//!
//! ## 依赖
//! - `crate::core::task_analyzer::TaskKind`: 领域路由
//! - `crate::core::preflight::PreflightChecker`: Phase 1 复用
//! - `crate::core::inertia::InertiaDetector`: Phase 11 维度之一
//! - `crate::core::humanizer::AIPatternDetector`: Phase 14 维度之一
//!
//! ## 引用关系
//! - 被 `workflow_engine::WorkflowEngine` 驱动
//! - 被 `CoreLoop::process_turn()` 在复杂度超阈值时激活

use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::core::task_analyzer::TaskKind;

// ─── 15 阶段状态机 ─────────────────────────────────────────────────────────

/// 工作流质量状态机 — 约束整个任务生命周期
///
/// ## 三大区域
/// - Pre-Execution (1-8): 需求理解 → 方案定稿
/// - Execution (9-10): 执行构思 → 执行
/// - Post-Execution (11-15): 多视角渐进式审查
///
/// ## 状态转换
/// 线性主路径 + 回退路径（审查不通过可回退到 SolutionDesign）
/// Fast-path: 简单任务 Comprehension → FastPathCompleted
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkflowPhase {
    // ═══ Pre-Execution Zone ═══
    /// 1. 需求审题：确认理解用户意图（消歧义、补隐含需求）
    Comprehension,
    /// 2. 需求梳理：拆解为结构化子需求 + 边界条件 + 约束
    Decomposition,
    /// 3. 需求方案：生成候选方案（≥2 可选路径）
    SolutionDesign,
    /// 4. 方案内部审查：单方案内部一致性、完整性、可行性
    InternalReview,
    /// 5. 方案关联审查：与现有代码/架构/约定的兼容性
    CrossReferenceReview,
    /// 6. 连环影响审查：变更波及范围（依赖链、下游消费方）
    CascadeImpact,
    /// 7. ROI 评估：收益 vs 成本 vs 风险三角
    RoiEvaluation,
    /// 8. 方案终版：确认最终方案 + 用户审批点
    FinalSolution,

    // ═══ Execution Zone ═══
    /// 9. 执行构思：拆解为可验证的执行步骤 + 回退方案
    ExecutionPlanning,
    /// 10. 执行：实际调用工具/生成代码/产出内容
    Execution,

    // ═══ Post-Execution Zone ═══
    /// 11. 多视角审查：按领域动态选择审查维度
    MultiPerspectiveReview,
    /// 12. 渐进式分层审查：表层→结构→语义→系统层层递进
    ProgressiveLayerReview,
    /// 13. 对抗性压力测试：模拟恶意输入/边界条件/故障注入
    AdversarialStress,
    /// 14. 用户感知验证：输出是否对用户有价值 + 可理解
    UserPerceptionCheck,
    /// 15. 长期维护评估：可维护性/可扩展性/技术债务
    MaintenanceAssessment,

    // ═══ Terminal ═══
    /// 完成（所有审查通过）
    Completed { summary: String },
    /// 失败（某阶段不通过且无法修复）
    Failed { phase: String, reason: String },
    /// 简单任务直通完成
    FastPathCompleted,
}

impl WorkflowPhase {
    /// 是否为终态（不可再转换）
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed { .. } | Self::FastPathCompleted)
    }

    /// 阶段编号（用于排序和进度计算）
    pub fn ordinal(&self) -> u8 {
        match self {
            Self::Comprehension => 1,
            Self::Decomposition => 2,
            Self::SolutionDesign => 3,
            Self::InternalReview => 4,
            Self::CrossReferenceReview => 5,
            Self::CascadeImpact => 6,
            Self::RoiEvaluation => 7,
            Self::FinalSolution => 8,
            Self::ExecutionPlanning => 9,
            Self::Execution => 10,
            Self::MultiPerspectiveReview => 11,
            Self::ProgressiveLayerReview => 12,
            Self::AdversarialStress => 13,
            Self::UserPerceptionCheck => 14,
            Self::MaintenanceAssessment => 15,
            Self::Completed { .. } | Self::Failed { .. } | Self::FastPathCompleted => 16,
        }
    }

    /// 下一个阶段（线性主路径）
    pub fn next(&self) -> Option<WorkflowPhase> {
        match self {
            Self::Comprehension => Some(Self::Decomposition),
            Self::Decomposition => Some(Self::SolutionDesign),
            Self::SolutionDesign => Some(Self::InternalReview),
            Self::InternalReview => Some(Self::CrossReferenceReview),
            Self::CrossReferenceReview => Some(Self::CascadeImpact),
            Self::CascadeImpact => Some(Self::RoiEvaluation),
            Self::RoiEvaluation => Some(Self::FinalSolution),
            Self::FinalSolution => Some(Self::ExecutionPlanning),
            Self::ExecutionPlanning => Some(Self::Execution),
            Self::Execution => Some(Self::MultiPerspectiveReview),
            Self::MultiPerspectiveReview => Some(Self::ProgressiveLayerReview),
            Self::ProgressiveLayerReview => Some(Self::AdversarialStress),
            Self::AdversarialStress => Some(Self::UserPerceptionCheck),
            Self::UserPerceptionCheck => Some(Self::MaintenanceAssessment),
            Self::MaintenanceAssessment => None, // → Completed (handled externally)
            _ => None,
        }
    }

    /// 阶段名称（日志/事件用）
    pub fn name(&self) -> &'static str {
        match self {
            Self::Comprehension => "comprehension",
            Self::Decomposition => "decomposition",
            Self::SolutionDesign => "solution_design",
            Self::InternalReview => "internal_review",
            Self::CrossReferenceReview => "cross_reference_review",
            Self::CascadeImpact => "cascade_impact",
            Self::RoiEvaluation => "roi_evaluation",
            Self::FinalSolution => "final_solution",
            Self::ExecutionPlanning => "execution_planning",
            Self::Execution => "execution",
            Self::MultiPerspectiveReview => "multi_perspective_review",
            Self::ProgressiveLayerReview => "progressive_layer_review",
            Self::AdversarialStress => "adversarial_stress",
            Self::UserPerceptionCheck => "user_perception_check",
            Self::MaintenanceAssessment => "maintenance_assessment",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::FastPathCompleted => "fast_path_completed",
        }
    }

    /// 所属区域
    pub fn zone(&self) -> WorkflowZone {
        match self.ordinal() {
            1..=8 => WorkflowZone::PreExecution,
            9..=10 => WorkflowZone::Execution,
            11..=15 => WorkflowZone::PostExecution,
            _ => WorkflowZone::Terminal,
        }
    }

    /// 转换验证：检查从当前状态到目标状态是否合法
    pub fn can_transition_to(&self, next: &WorkflowPhase) -> bool {
        // 任何非终态 → Failed 始终合法
        if matches!(next, Self::Failed { .. }) && !self.is_terminal() {
            return true;
        }
        // 终态不可转换
        if self.is_terminal() {
            return false;
        }

        match (self, next) {
            // ─── 线性主路径 ─────────────────────────────
            (Self::Comprehension, Self::Decomposition) => true,
            (Self::Decomposition, Self::SolutionDesign) => true,
            (Self::SolutionDesign, Self::InternalReview) => true,
            (Self::InternalReview, Self::CrossReferenceReview) => true,
            (Self::CrossReferenceReview, Self::CascadeImpact) => true,
            (Self::CascadeImpact, Self::RoiEvaluation) => true,
            (Self::RoiEvaluation, Self::FinalSolution) => true,
            (Self::FinalSolution, Self::ExecutionPlanning) => true,
            (Self::ExecutionPlanning, Self::Execution) => true,
            (Self::Execution, Self::MultiPerspectiveReview) => true,
            (Self::MultiPerspectiveReview, Self::ProgressiveLayerReview) => true,
            (Self::ProgressiveLayerReview, Self::AdversarialStress) => true,
            (Self::AdversarialStress, Self::UserPerceptionCheck) => true,
            (Self::UserPerceptionCheck, Self::MaintenanceAssessment) => true,
            (Self::MaintenanceAssessment, Self::Completed { .. }) => true,

            // ─── Fast-path ──────────────────────────────
            (Self::Comprehension, Self::FastPathCompleted) => true,

            // ─── 回退路径（审查不通过 → 回到方案设计）──
            (Self::InternalReview, Self::SolutionDesign) => true,
            (Self::CrossReferenceReview, Self::SolutionDesign) => true,
            (Self::CascadeImpact, Self::SolutionDesign) => true,
            (Self::RoiEvaluation, Self::SolutionDesign) => true,

            // ─── 后置审查回退（修复后重新执行）─────────
            (Self::MultiPerspectiveReview, Self::Execution) => true,
            (Self::AdversarialStress, Self::Execution) => true,

            // ─── 跳跃路径（Lite 模式跳过中间阶段）─────
            // InternalReview → FinalSolution (跳过 5-7)
            (Self::InternalReview, Self::FinalSolution) => true,
            // Execution → Completed (跳过 12-15)
            (Self::MultiPerspectiveReview, Self::Completed { .. }) => true,

            _ => false,
        }
    }
}

/// 工作流区域
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowZone {
    PreExecution,
    Execution,
    PostExecution,
    Terminal,
}

// ─── 审查结果 ─────────────────────────────────────────────────────────────

/// 阶段审查结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PhaseVerdict {
    /// 通过，进入下一阶段
    Pass { findings: Vec<String> },
    /// 需要回退到指定阶段
    Rollback { to_phase: String, reason: String },
    /// 失败，不可恢复
    Fail { reason: String },
    /// 跳过（配置允许 + 复杂度不足）
    Skip,
}

impl PhaseVerdict {
    pub fn is_pass(&self) -> bool { matches!(self, Self::Pass { .. }) }
    pub fn is_rollback(&self) -> bool { matches!(self, Self::Rollback { .. }) }
}

// ─── 领域审查维度 ─────────────────────────────────────────────────────────

/// 领域特定的审查维度集（Phase 11/12/13 按 TaskKind 动态选择）
///
/// 每个领域有 4 个核心审查维度，审查器按维度逐一评估。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDimensions {
    pub dimensions: Vec<ReviewDimension>,
}

/// 单个审查维度
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDimension {
    pub name: String,
    pub description: String,
    /// 该维度的权重 [0, 1]
    pub weight: f64,
    /// 通过阈值 [0, 1]，低于此值则该维度不通过
    pub threshold: f64,
}

impl ReviewDimensions {
    /// 按 TaskKind 选择审查维度集（Phase 11: 多视角审查）
    pub fn for_multi_perspective(kind: &TaskKind) -> Self {
        match kind {
            TaskKind::CodeWriting | TaskKind::CodeReading | TaskKind::FileEdit => Self {
                dimensions: vec![
                    dim("security", "安全性：注入/越权/信息泄露", 0.30, 0.7),
                    dim("performance", "性能：时间复杂度/内存/并发", 0.25, 0.6),
                    dim("correctness", "正确性：逻辑/边界/类型", 0.30, 0.8),
                    dim("readability", "可读性：命名/结构/注释", 0.15, 0.5),
                ],
            },
            TaskKind::Debugging => Self {
                dimensions: vec![
                    dim("root_cause", "根因定位：是否找到真正原因", 0.35, 0.8),
                    dim("fix_correctness", "修复正确性：修复是否引入新问题", 0.30, 0.8),
                    dim("regression", "回归风险：相关代码路径是否安全", 0.20, 0.7),
                    dim("reproducibility", "可复现：修复后问题是否确实消失", 0.15, 0.6),
                ],
            },
            TaskKind::Architecture => Self {
                dimensions: vec![
                    dim("scalability", "可扩展性：支持未来增长", 0.25, 0.6),
                    dim("coupling", "耦合度：模块间依赖是否合理", 0.25, 0.7),
                    dim("consistency", "一致性：与现有架构风格一致", 0.25, 0.7),
                    dim("ops_cost", "运维成本：部署/监控/故障恢复", 0.25, 0.5),
                ],
            },
            TaskKind::DataAnalysis => Self {
                dimensions: vec![
                    dim("data_quality", "数据质量：完整/准确/无重复", 0.30, 0.8),
                    dim("consistency", "一致性：跨源/跨时间一致", 0.25, 0.7),
                    dim("timeliness", "时效性：数据是否最新", 0.20, 0.6),
                    dim("coverage", "覆盖度：是否覆盖所有场景", 0.25, 0.7),
                ],
            },
            TaskKind::Mathematics => Self {
                dimensions: vec![
                    dim("logical_completeness", "逻辑完备：推导链无跳跃", 0.35, 0.9),
                    dim("boundary_conditions", "边界条件：极值/零值/溢出", 0.25, 0.8),
                    dim("numerical_stability", "数值稳定性：精度/收敛", 0.25, 0.7),
                    dim("generalization", "泛化性：结论适用范围", 0.15, 0.6),
                ],
            },
            TaskKind::Linguistics => Self {
                dimensions: vec![
                    dim("factual_accuracy", "事实准确：无捏造/无错误引用", 0.35, 0.9),
                    dim("tone_fit", "语气适配：与目标受众/场景匹配", 0.25, 0.7),
                    dim("audience_match", "受众匹配：难度/术语/深度合适", 0.20, 0.6),
                    dim("completeness", "完整性：要点无遗漏", 0.20, 0.7),
                ],
            },
            TaskKind::WebSearch | TaskKind::KnowledgeQuery => Self {
                dimensions: vec![
                    dim("source_reliability", "来源可靠：权威/可验证", 0.30, 0.7),
                    dim("recency", "时效性：信息是否过时", 0.25, 0.6),
                    dim("relevance", "相关性：与需求匹配度", 0.25, 0.7),
                    dim("synthesis", "综合质量：多源整合有结论", 0.20, 0.6),
                ],
            },
            // Review 任务（含文案审查）
            TaskKind::Review => Self {
                dimensions: vec![
                    dim("thoroughness", "全面性：覆盖所有变更点", 0.30, 0.8),
                    dim("actionability", "可操作性：建议具体可执行", 0.25, 0.7),
                    dim("priority", "优先级：P0/P1/P2 分级合理", 0.25, 0.6),
                    dim("constructiveness", "建设性：非纯批评有解决方案", 0.20, 0.6),
                ],
            },
            // 通用对话/未分类
            TaskKind::GeneralChat => Self {
                dimensions: vec![
                    dim("relevance", "相关性：回答是否切题", 0.30, 0.7),
                    dim("accuracy", "准确性：事实/推理无误", 0.30, 0.8),
                    dim("completeness", "完整性：要点无遗漏", 0.20, 0.6),
                    dim("clarity", "清晰度：表达简洁明了", 0.20, 0.6),
                ],
            },
        }
    }

    /// Phase 12: 渐进式分层审查（4 层递进，所有领域通用 + 领域微调）
    pub fn for_progressive_layer(kind: &TaskKind) -> Self {
        let domain_label = match kind {
            TaskKind::CodeWriting | TaskKind::CodeReading | TaskKind::FileEdit
                | TaskKind::Debugging => "代码",
            TaskKind::DataAnalysis => "数据",
            TaskKind::Mathematics => "公式",
            TaskKind::Linguistics | TaskKind::Review => "文本",
            _ => "内容",
        };
        Self {
            dimensions: vec![
                dim("surface", &format!("表层：{domain_label}格式/拼写/语法"), 0.15, 0.8),
                dim("structure", &format!("结构：{domain_label}组织/流程/依赖"), 0.25, 0.7),
                dim("semantic", &format!("语义：{domain_label}逻辑/意图/正确性"), 0.35, 0.7),
                dim("system", &format!("系统：{domain_label}在系统中的影响/一致性"), 0.25, 0.6),
            ],
        }
    }

    /// Phase 13: 对抗性压力测试维度
    pub fn for_adversarial(kind: &TaskKind) -> Self {
        match kind {
            TaskKind::CodeWriting | TaskKind::CodeReading | TaskKind::FileEdit => Self {
                dimensions: vec![
                    dim("injection", "注入攻击：SQL/XSS/命令注入", 0.30, 0.9),
                    dim("edge_cases", "边界条件：空/null/溢出/并发", 0.30, 0.8),
                    dim("failure_modes", "故障模式：网络断开/超时/资源耗尽", 0.25, 0.7),
                    dim("malicious_input", "恶意输入：超长/特殊字符/格式错误", 0.15, 0.7),
                ],
            },
            TaskKind::DataAnalysis => Self {
                dimensions: vec![
                    dim("dirty_data", "脏数据：缺失/异常/重复/格式错误", 0.35, 0.8),
                    dim("scale", "规模压力：10x/100x 数据量", 0.25, 0.6),
                    dim("temporal", "时间异常：乱序/重复/跨时区", 0.20, 0.7),
                    dim("adversarial_records", "对抗记录：伪造/注入/投毒", 0.20, 0.7),
                ],
            },
            TaskKind::Linguistics | TaskKind::Review => Self {
                dimensions: vec![
                    dim("factual_challenge", "事实挑战：断言是否经得起反驳", 0.35, 0.8),
                    dim("logical_holes", "逻辑漏洞：推理链是否完整", 0.30, 0.7),
                    dim("bias_detection", "偏见检测：是否有未声明的立场", 0.20, 0.6),
                    dim("misinterpretation", "误解风险：读者可能的歧义理解", 0.15, 0.6),
                ],
            },
            _ => Self {
                dimensions: vec![
                    dim("edge_cases", "边界条件", 0.30, 0.7),
                    dim("failure_modes", "故障模式", 0.30, 0.7),
                    dim("contradictions", "内部矛盾", 0.20, 0.7),
                    dim("assumptions", "隐含假设是否成立", 0.20, 0.6),
                ],
            },
        }
    }
}

fn dim(name: &str, desc: &str, weight: f64, threshold: f64) -> ReviewDimension {
    ReviewDimension {
        name: name.to_string(),
        description: desc.to_string(),
        weight,
        threshold,
    }
}

// ─── 阶段配置 ─────────────────────────────────────────────────────────────

/// 工作流路由配置
#[derive(Debug, Clone)]
pub struct PhaseConfig {
    /// 任务复杂度阈值——低于此值走 fast-path（默认 0.3）
    pub fast_path_threshold: f64,
    /// Lite 模式阈值（0.3~此值走 Lite 跳过部分阶段）（默认 0.6）
    pub full_threshold: f64,
    /// 最大回退次数（防止无限循环）
    pub max_rollbacks: u32,
    /// 最大总执行时间（秒）
    pub max_total_duration_secs: u64,
}

impl Default for PhaseConfig {
    fn default() -> Self {
        Self {
            fast_path_threshold: 0.3,
            full_threshold: 0.6,
            max_rollbacks: 10,
            max_total_duration_secs: 3600,
        }
    }
}

/// 复杂度等级决定走哪条路径
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowMode {
    /// score < 0.3: 跳过全部审查，直接执行
    FastPath,
    /// score 0.3~0.6: 跳过 5-7 (CrossRef/Cascade/ROI) + 12-15 (后置深度审查)
    Lite,
    /// score > 0.6: 全 15 阶段
    Full,
}

impl WorkflowMode {
    pub fn from_complexity(score: f64, config: &PhaseConfig) -> Self {
        if score < config.fast_path_threshold {
            Self::FastPath
        } else if score < config.full_threshold {
            Self::Lite
        } else {
            Self::Full
        }
    }

    /// 在此模式下，指定阶段是否应跳过
    pub fn should_skip(&self, phase: &WorkflowPhase) -> bool {
        match self {
            Self::FastPath => true, // 全跳（由 engine 直接走 fast-path terminal）
            Self::Lite => matches!(phase,
                WorkflowPhase::CrossReferenceReview
                | WorkflowPhase::CascadeImpact
                | WorkflowPhase::RoiEvaluation
                | WorkflowPhase::ProgressiveLayerReview
                | WorkflowPhase::AdversarialStress
                | WorkflowPhase::UserPerceptionCheck
                | WorkflowPhase::MaintenanceAssessment
            ),
            Self::Full => false, // 全跑
        }
    }
}

// ─── 审查器 trait ─────────────────────────────────────────────────────────

/// 阶段审查器接口
///
/// 每个阶段实现此 trait。审查器根据 WorkflowContext 中的累积数据 +
/// TaskKind 选择领域特定的审查逻辑。
///
/// ## 生命周期
/// 审查器无状态（每次调用独立），WorkflowContext 是跨阶段状态载体。
#[async_trait::async_trait]
pub trait PhaseChecker: Send + Sync {
    /// 执行审查，返回 verdict
    async fn check(&self, ctx: &WorkflowContext) -> PhaseVerdict;
    /// 阶段名称
    fn phase_name(&self) -> &'static str;
    /// 该审查器适用的 phase
    fn target_phase(&self) -> WorkflowPhase;
}

// ─── 输出分段 ─────────────────────────────────────────────────────────────

/// 输出分段（大输出按语义边界拆分，供审查器分块处理）
///
/// ## 分段策略
/// - 代码：按函数/struct/impl 边界
/// - 文本：按 heading/段落边界
/// - 混合：按空行分隔块
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSegment {
    /// 段序号（从 0 开始）
    pub index: usize,
    /// 段内容
    pub content: String,
    /// 段类型提示（code/text/mixed）
    pub segment_type: String,
    /// 起始字符偏移（相对于完整输出）
    pub offset: usize,
}

// ─── 工作流上下文 ─────────────────────────────────────────────────────────

/// 候选方案
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Solution {
    pub id: String,
    pub description: String,
    pub approach: String,
    pub pros: Vec<String>,
    pub cons: Vec<String>,
    pub estimated_effort: String,
}

/// 审查发现
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub dimension: String,
    pub severity: FindingSeverity,
    pub description: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindingSeverity {
    Critical,  // 必须修复才能通过
    Major,     // 应该修复
    Minor,     // 建议改进
    Info,      // 信息性
}

/// 工作流上下文（跨阶段累积数据）
///
/// ## 生命周期
/// - 创建于 WorkflowEngine::new()
/// - 每个阶段向其中写入数据
/// - 回退时保留历史（不清除）
/// - 终态后冻结
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowContext {
    // 输入
    pub input: String,
    pub task_kind: TaskKind,
    pub complexity_score: f64,
    pub mode: String, // "fast_path" | "lite" | "full"

    // Phase 1: Comprehension
    pub confirmed_intent: Option<String>,
    pub ambiguities: Vec<String>,

    // Phase 2: Decomposition
    pub sub_requirements: Vec<String>,
    pub constraints: Vec<String>,
    pub boundary_conditions: Vec<String>,

    // Phase 3: SolutionDesign
    pub candidate_solutions: Vec<Solution>,
    pub selected_solution_id: Option<String>,

    // Phase 4-7: Reviews
    pub internal_findings: Vec<ReviewFinding>,
    pub cross_ref_findings: Vec<ReviewFinding>,
    pub cascade_affected: Vec<String>,
    pub roi_verdict: Option<RoiVerdict>,

    // Phase 8: FinalSolution
    pub approved_solution: Option<Solution>,

    // Phase 9: ExecutionPlanning
    pub execution_steps: Vec<String>,
    pub rollback_plan: Option<String>,

    // Phase 10: Execution
    pub execution_output: Option<String>,
    /// 大输出分段（当 execution_output > 32KB 时按语义边界分割）
    /// 审查器可逐段检查而非扫描全文，提升审查精度和效率
    pub output_segments: Vec<OutputSegment>,
    pub tool_outputs: Vec<serde_json::Value>,

    // Phase 11-15: Post-execution
    pub multi_perspective_scores: HashMap<String, f64>,
    pub layer_review_findings: Vec<ReviewFinding>,
    pub adversarial_findings: Vec<ReviewFinding>,
    pub user_perception_score: Option<f64>,
    pub maintenance_score: Option<f64>,

    // 元数据
    pub phase_history: Vec<PhaseHistoryEntry>,
    pub rollback_count: u32,
    pub started_at: i64,
}

/// ROI 评估结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoiVerdict {
    pub benefit_score: f64,
    pub cost_score: f64,
    pub risk_score: f64,
    /// benefit / (cost + risk) 的综合评判
    pub ratio: f64,
    /// 最终判定
    pub approved: bool,
    pub rationale: String,
}

/// 阶段历史记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseHistoryEntry {
    pub phase: String,
    pub verdict: String,
    pub duration_ms: u64,
    pub findings_count: usize,
    pub timestamp: i64,
}

impl WorkflowContext {
    pub fn new(input: String, task_kind: TaskKind, complexity_score: f64, mode: WorkflowMode) -> Self {
        Self {
            input,
            task_kind,
            complexity_score,
            mode: match mode {
                WorkflowMode::FastPath => "fast_path".into(),
                WorkflowMode::Lite => "lite".into(),
                WorkflowMode::Full => "full".into(),
            },
            confirmed_intent: None,
            ambiguities: Vec::new(),
            sub_requirements: Vec::new(),
            constraints: Vec::new(),
            boundary_conditions: Vec::new(),
            candidate_solutions: Vec::new(),
            selected_solution_id: None,
            internal_findings: Vec::new(),
            cross_ref_findings: Vec::new(),
            cascade_affected: Vec::new(),
            roi_verdict: None,
            approved_solution: None,
            execution_steps: Vec::new(),
            rollback_plan: None,
            execution_output: None,
            output_segments: Vec::new(),
            tool_outputs: Vec::new(),
            multi_perspective_scores: HashMap::new(),
            layer_review_findings: Vec::new(),
            adversarial_findings: Vec::new(),
            user_perception_score: None,
            maintenance_score: None,
            phase_history: Vec::new(),
            rollback_count: 0,
            started_at: chrono::Utc::now().timestamp(),
        }
    }

    /// 获取已选方案（Phase 3 之后可用）
    pub fn selected_solution(&self) -> Option<&Solution> {
        let id = self.selected_solution_id.as_ref()?;
        self.candidate_solutions.iter().find(|s| &s.id == id)
    }

    /// 是否有 Critical 级别的未解决发现
    pub fn has_critical_findings(&self) -> bool {
        self.internal_findings.iter().any(|f| f.severity == FindingSeverity::Critical)
            || self.cross_ref_findings.iter().any(|f| f.severity == FindingSeverity::Critical)
            || self.adversarial_findings.iter().any(|f| f.severity == FindingSeverity::Critical)
    }

    /// 计算总进度 [0, 1]
    pub fn progress(&self) -> f64 {
        if self.phase_history.is_empty() { return 0.0; }
        let max_phase = 15.0;
        let completed = self.phase_history.len() as f64;
        (completed / max_phase).min(1.0)
    }
}

// ─── 事件系统 ─────────────────────────────────────────────────────────────

/// 工作流事件（通过 broadcast channel 发送，供 UI/日志消费）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkflowEvent {
    PhaseEntered { phase: String, zone: String, timestamp: i64 },
    PhaseCompleted { phase: String, verdict: String, duration_ms: u64, findings_count: usize },
    RollbackTriggered { from: String, to: String, reason: String },
    WorkflowCompleted { total_phases: usize, rollbacks: u32, duration_ms: u64, mode: String },
    WorkflowFailed { at_phase: String, reason: String },
    FastPathActivated { reason: String },
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linear_path() {
        let phases = vec![
            WorkflowPhase::Comprehension,
            WorkflowPhase::Decomposition,
            WorkflowPhase::SolutionDesign,
            WorkflowPhase::InternalReview,
            WorkflowPhase::CrossReferenceReview,
            WorkflowPhase::CascadeImpact,
            WorkflowPhase::RoiEvaluation,
            WorkflowPhase::FinalSolution,
            WorkflowPhase::ExecutionPlanning,
            WorkflowPhase::Execution,
            WorkflowPhase::MultiPerspectiveReview,
            WorkflowPhase::ProgressiveLayerReview,
            WorkflowPhase::AdversarialStress,
            WorkflowPhase::UserPerceptionCheck,
            WorkflowPhase::MaintenanceAssessment,
        ];
        for window in phases.windows(2) {
            assert!(
                window[0].can_transition_to(&window[1]),
                "{:?} should transition to {:?}", window[0], window[1]
            );
        }
        // Last → Completed
        assert!(WorkflowPhase::MaintenanceAssessment.can_transition_to(
            &WorkflowPhase::Completed { summary: "done".into() }
        ));
    }

    #[test]
    fn test_rollback_paths() {
        assert!(WorkflowPhase::InternalReview.can_transition_to(&WorkflowPhase::SolutionDesign));
        assert!(WorkflowPhase::CrossReferenceReview.can_transition_to(&WorkflowPhase::SolutionDesign));
        assert!(WorkflowPhase::CascadeImpact.can_transition_to(&WorkflowPhase::SolutionDesign));
        assert!(WorkflowPhase::RoiEvaluation.can_transition_to(&WorkflowPhase::SolutionDesign));
        assert!(WorkflowPhase::MultiPerspectiveReview.can_transition_to(&WorkflowPhase::Execution));
        assert!(WorkflowPhase::AdversarialStress.can_transition_to(&WorkflowPhase::Execution));
    }

    #[test]
    fn test_invalid_transitions() {
        // 不能跳跃
        assert!(!WorkflowPhase::Comprehension.can_transition_to(&WorkflowPhase::Execution));
        // 终态不可转换
        assert!(!WorkflowPhase::Completed { summary: "x".into() }.can_transition_to(&WorkflowPhase::Comprehension));
        assert!(!WorkflowPhase::FastPathCompleted.can_transition_to(&WorkflowPhase::Execution));
    }

    #[test]
    fn test_fast_path() {
        assert!(WorkflowPhase::Comprehension.can_transition_to(&WorkflowPhase::FastPathCompleted));
        // Other phases cannot go to fast-path
        assert!(!WorkflowPhase::SolutionDesign.can_transition_to(&WorkflowPhase::FastPathCompleted));
    }

    #[test]
    fn test_any_to_failed() {
        assert!(WorkflowPhase::Comprehension.can_transition_to(
            &WorkflowPhase::Failed { phase: "x".into(), reason: "y".into() }
        ));
        assert!(WorkflowPhase::Execution.can_transition_to(
            &WorkflowPhase::Failed { phase: "x".into(), reason: "y".into() }
        ));
    }

    #[test]
    fn test_workflow_mode() {
        let config = PhaseConfig::default();
        assert_eq!(WorkflowMode::from_complexity(0.1, &config), WorkflowMode::FastPath);
        assert_eq!(WorkflowMode::from_complexity(0.4, &config), WorkflowMode::Lite);
        assert_eq!(WorkflowMode::from_complexity(0.8, &config), WorkflowMode::Full);
    }

    #[test]
    fn test_lite_skips() {
        let lite = WorkflowMode::Lite;
        assert!(lite.should_skip(&WorkflowPhase::CrossReferenceReview));
        assert!(lite.should_skip(&WorkflowPhase::CascadeImpact));
        assert!(lite.should_skip(&WorkflowPhase::RoiEvaluation));
        assert!(!lite.should_skip(&WorkflowPhase::Comprehension));
        assert!(!lite.should_skip(&WorkflowPhase::Execution));
        assert!(!lite.should_skip(&WorkflowPhase::MultiPerspectiveReview));
    }

    #[test]
    fn test_review_dimensions_by_kind() {
        let code_dims = ReviewDimensions::for_multi_perspective(&TaskKind::CodeWriting);
        assert_eq!(code_dims.dimensions.len(), 4);
        assert_eq!(code_dims.dimensions[0].name, "security");

        let data_dims = ReviewDimensions::for_multi_perspective(&TaskKind::DataAnalysis);
        assert_eq!(data_dims.dimensions[0].name, "data_quality");

        let math_dims = ReviewDimensions::for_multi_perspective(&TaskKind::Mathematics);
        assert_eq!(math_dims.dimensions[0].name, "logical_completeness");
    }

    #[test]
    fn test_ordinal_ordering() {
        assert!(WorkflowPhase::Comprehension.ordinal() < WorkflowPhase::Execution.ordinal());
        assert!(WorkflowPhase::Execution.ordinal() < WorkflowPhase::MaintenanceAssessment.ordinal());
    }

    #[test]
    fn test_next_chain() {
        let mut phase = WorkflowPhase::Comprehension;
        let mut count = 0;
        while let Some(next) = phase.next() {
            phase = next;
            count += 1;
        }
        // 14 transitions (1→2→...→15, then next() returns None)
        assert_eq!(count, 14);
        assert_eq!(phase, WorkflowPhase::MaintenanceAssessment);
    }

    #[test]
    fn test_lite_skip_jump() {
        // Lite mode: InternalReview → FinalSolution (跳过 5-7)
        assert!(WorkflowPhase::InternalReview.can_transition_to(&WorkflowPhase::FinalSolution));
        // Lite mode: MultiPerspective → Completed (跳过 12-15)
        assert!(WorkflowPhase::MultiPerspectiveReview.can_transition_to(
            &WorkflowPhase::Completed { summary: "lite done".into() }
        ));
    }
}
