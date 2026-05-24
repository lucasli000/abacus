//! Progressive Output Protocol — L0 类型定义
//!
//! ## 场景
//! 定义渐进式输出人机缓冲层的所有数据结构。
//! 由 L2 Engine 层（progressive_gate / progressive / progressive_inject）使用。
//!
//! ## 依赖链
//! 无外部依赖，纯类型定义。
//!
//! ## 引用关系
//! - 被 abacus-core::core::progressive* 全部引用
//! - 被 abacus-core::core::task_analyzer 引用（ComplexityProfile）
//! - 被 abacus-orchestrator::subagent 引用（GateScope）
//! - 被 abacus-core::config 引用（AutonomyLevel）

use serde::{Deserialize, Serialize};
use std::time::Duration;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 输出策略
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 渐进输出策略
///
/// ## 场景
/// ProgressiveGate 根据任务复杂度选择策略，决定 LLM 的输出行为模式。
///
/// ## 与 OutputMode 的区别
/// OutputMode (Focused/Split/Verbose) 控制"渲染通道"——怎么展示给用户。
/// OutputStrategy 控制"生成节奏"——LLM 什么时候该停、该确认、该续写。
/// 两者正交：Staged 策略下也可以用 Focused 渲染。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OutputStrategy {
    /// 直通模式：简单任务，LLM 一次性输出，无门控
    PassThrough,

    /// 分段模式：中等任务，输出结构化分段，不阻塞但可分段渲染
    Staged {
        sections: Vec<SectionPlan>,
    },

    /// 门控模式：复杂任务，先输出确认清单，等人工确认后再续写
    Gated {
        checklist: Checklist,
    },
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 确认清单
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 确认清单：人机缓冲层的核心数据结构
///
/// ## 场景
/// LLM 生成长文档前，先输出此清单让用户确认方向。
/// 分为上区（信息确认）和下区（决策面板）。
///
/// ## 边界
/// - items 为空时视为无效清单，触发降级
/// - timeout 为 None 时永不超时（Low 模式）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Checklist {
    /// 上区：信息确认项
    pub info_items: Vec<ChecklistItem>,
    /// 下区：决策块（独立展示）
    pub decisions: Vec<DecisionBlock>,
    /// 清单生成的上下文摘要
    pub context_digest: String,
    /// 超时策略
    pub timeout: Option<Duration>,
    /// 是否阻塞（true = 必须等用户确认）
    pub blocking: bool,
}

impl Checklist {
    /// 创建占位清单（Gate 决策时用，后由 LLM 填充）
    ///
    /// ## 引用关系
    /// - 被 `from_task()` 取代作为首选；保留用于无上下文时的降级入口
    /// - 消费方：ProgressiveGate::decide*（已迁移至 from_task）
    pub fn placeholder() -> Self {
        Self {
            info_items: Vec::new(),
            decisions: Vec::new(),
            context_digest: String::new(),
            timeout: Some(Duration::from_secs(300)),
            blocking: true,
        }
    }

    /// 基于任务描述和复杂度分数生成有意义的验证清单
    ///
    /// ## 引用关系
    /// - 被 ProgressiveGate::decide / decide_with_task_type / decide_post_execution 调用
    /// - 依赖 ChecklistItem / ChecklistCategory
    ///
    /// ## 生成规则
    /// - 始终添加"验证输出正确性"项
    /// - complexity > 0.5 时追加"副作用检查"项
    /// - complexity > 0.7 时追加"测试覆盖确认"项
    pub fn from_task(task_description: &str, complexity_score: f64) -> Self {
        let mut items = Vec::new();
        let mut next_id: u32 = 1;

        // 始终添加：验证输出是否符合目标
        items.push(ChecklistItem {
            id: next_id,
            category: ChecklistCategory::NeedsVerification,
            label: "Verify output matches the stated goal".into(),
            detail: if task_description.is_empty() {
                None
            } else {
                Some(format!("Task: {}", task_description))
            },
            source: None,
            response: None,
        });
        next_id += 1;

        // complexity > 0.5：检查副作用
        if complexity_score > 0.5 {
            items.push(ChecklistItem {
                id: next_id,
                category: ChecklistCategory::NeedsVerification,
                label: "Check for unintended side effects on related components".into(),
                detail: None,
                source: None,
                response: None,
            });
            next_id += 1;
        }

        // complexity > 0.7：确认测试覆盖
        if complexity_score > 0.7 {
            items.push(ChecklistItem {
                id: next_id,
                category: ChecklistCategory::NeedsVerification,
                label: "Confirm test coverage for changed behavior".into(),
                detail: None,
                source: None,
                response: None,
            });
            let _ = next_id; // suppress unused warning on last increment
        }

        Self {
            info_items: items,
            decisions: Vec::new(),
            context_digest: if task_description.is_empty() {
                "Gate triggered by complexity threshold".into()
            } else {
                format!("Gate triggered for: {}", task_description)
            },
            timeout: Some(Duration::from_secs(300)),
            blocking: true,
        }
    }

    /// 清单是否有效（至少有 1 项内容）
    pub fn is_valid(&self) -> bool {
        !self.info_items.is_empty() || !self.decisions.is_empty()
    }
}

/// 信息确认项
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChecklistItem {
    pub id: u32,
    pub category: ChecklistCategory,
    /// 人类可读描述
    pub label: String,
    /// 详细说明（可选）
    pub detail: Option<String>,
    /// 信息来源
    pub source: Option<String>,
    /// 用户响应
    pub response: Option<UserResponse>,
}

/// 清单项分类
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ChecklistCategory {
    /// 已获取的关键信息（仅告知，用户可纠正）
    InfoAcquired,
    /// 需要人工确认/补充的信息
    NeedsVerification,
    /// 风险/限制告知（仅通知）
    RiskNotice,
}

/// 决策块 — 底部独立展示
///
/// ## 场景
/// 每个需要用户选择的决策点独立为一个 block，
/// 含推荐选项 + 理由 + pros/cons 对比。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionBlock {
    pub id: u32,
    /// 决策问题（一句话）
    pub question: String,
    /// 决策背景（为什么需要做这个选择，≤30 字）
    pub context: String,
    /// 选项列表
    pub options: Vec<DecisionOption>,
    /// AI 推荐的选项 id
    pub recommended: Option<String>,
    /// 推荐原因（≤50 字，结论先行）
    pub recommendation_reason: Option<String>,
    /// 用户响应
    pub response: Option<UserResponse>,
}

/// 决策选项
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionOption {
    pub id: String,
    pub label: String,
    /// 一句话描述
    pub summary: String,
    /// 推荐理由
    pub rationale: Option<String>,
    pub pros: Vec<String>,
    pub cons: Vec<String>,
    /// 推荐度 [0.0, 1.0]
    pub confidence: f64,
}

/// 用户对清单项的响应
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UserResponse {
    /// 确认原内容
    Confirmed,
    /// 纠正（用户提供正确信息）
    Corrected(String),
    /// 选择（决策点）
    Chosen(String),
    /// 跳过（使用默认值）
    Skipped,
    /// 补充信息
    Supplemented(String),
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 分段计划
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 输出段落计划（Staged 模式）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SectionPlan {
    pub id: u32,
    pub title: String,
    /// 预估输出字符数
    pub estimated_chars: u32,
    /// 是否需要确认后才生成
    pub requires_confirmation: bool,
    pub status: SectionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SectionStatus {
    Planned,
    Generating,
    Completed,
    Skipped,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 状态机
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 渐进输出协议状态
///
/// ## 生命周期
/// Analyzing → StrategyDecided → (分支):
///   PassThrough → (无后续状态，直接透传)
///   Staged → Generating(section N) → Completed
///   Gated → AwaitingConfirmation → Generating → Completed
///   任意 → Aborted
///   Generating → ReconfirmRequested → AwaitingConfirmation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ProgressiveState {
    /// 正在分析任务复杂度
    Analyzing,

    /// 已决定策略
    StrategyDecided {
        strategy: OutputStrategy,
    },

    /// 清单已输出，等待用户确认
    AwaitingConfirmation {
        checklist: Checklist,
        emitted_at_epoch_ms: u64,
    },

    /// 用户要求修改已确认的决策（从 Generating 回退）
    ReconfirmRequested {
        prior_decisions: Vec<(u32, UserResponse)>,
        modification_ids: Vec<u32>,
    },

    /// 正在生成内容
    Generating {
        strategy: OutputStrategy,
        current_section: Option<u32>,
        confirmed_decisions: Vec<(u32, UserResponse)>,
    },

    /// 输出完成
    Completed {
        total_sections: u32,
        total_tokens: u64,
    },

    /// 用户主动中止
    Aborted {
        reason: String,
    },
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 输出动作
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 输出块处理动作（Controller → CoreLoop）
#[derive(Debug, Clone, PartialEq)]
pub enum OutputAction {
    /// 正常转发给用户
    Forward,
    /// 暂存不转发（还在组装清单）
    Buffer,
    /// 停止生成，进入等待确认状态
    Gate,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 自主执行程度
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 自主执行程度 — 用户配置项
///
/// ## 场景
/// 用户通过 Setting.yaml 或 CLI --autonomy 配置。
/// 映射为 (passthrough_threshold, gated_threshold) 对。
///
/// ## 与阈值的关系
/// AutonomyLevel 是面向用户的语义配置（High/Medium/Low）。
/// 内部通过 to_thresholds() 转为数值阈值。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum AutonomyLevel {
    /// 全托管：无门控 + 宫殿决策 + 推演兜底。适用于可信长期任务。
    /// - ProgressiveGate 全程 PassThrough
    /// - 任务路由经 classify_input() + recommend_next_tools()
    /// - 异常由 DeductionEngine 检测后降级
    /// - 结果自动回写双宫殿
    Full,
    /// 高自主：几乎不中断（仅极端复杂任务门控）
    High,
    /// 中自主（默认）：大型任务需用户确认
    #[default]
    Medium,
    /// 低自主：中型+大型均需确认
    Low,
}


impl AutonomyLevel {
    /// 映射为 (passthrough_threshold, gated_threshold)
    /// 全托管模式下：门控永远关闭（全放行）
    pub fn is_fully_managed(self) -> bool {
        matches!(self, AutonomyLevel::Full)
    }

    pub fn to_thresholds(self) -> (f64, f64) {
        match self {
            AutonomyLevel::Full => (1.0, 1.0),    // 全放行
            AutonomyLevel::High => (0.70, 0.90),
            AutonomyLevel::Medium => (0.30, 0.50), // Gated 从 0.70 降至 0.50：中等复杂度任务更早触发门控
            AutonomyLevel::Low => (0.30, 0.40),    // PT 从 0.15 升至 0.30：简单任务仍能 PassThrough
        }
    }

    /// 超时行为
    pub fn timeout_behavior(self) -> TimeoutBehavior {
        match self {
            AutonomyLevel::Full => TimeoutBehavior::AutoProceed { secs: 10 },
            AutonomyLevel::High => TimeoutBehavior::AutoProceed { secs: 60 },
            AutonomyLevel::Medium => TimeoutBehavior::AutoProceed { secs: 300 },
            AutonomyLevel::Low => TimeoutBehavior::WaitForever,
        }
    }

    /// 强制门控类型列表
    pub fn forced_gated_types(self) -> Vec<&'static str> {
        match self {
            AutonomyLevel::Full => vec![],                // 全托管：无强制门控
            AutonomyLevel::High => vec!["compliance_doc"],
            AutonomyLevel::Medium => vec![
                "prd", "sop", "architecture_design",
                "financial_report", "compliance_doc",
            ],
            AutonomyLevel::Low => vec![
                "prd", "sop", "architecture_design",
                "financial_report", "compliance_doc",
                "code_writing", "data_analysis",
                "review", "debugging",
            ],
        }
    }
}

/// 超时后行为
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TimeoutBehavior {
    /// 超时后自动用默认值续写
    AutoProceed { secs: u64 },
    /// 永远等待用户确认
    WaitForever,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 复杂度剖面
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 复杂度剖面 — 7 维信号
///
/// ## 场景
/// TaskAnalyzer::analyze_complexity() 的输出。
/// ProgressiveGate 读取此结构决定输出策略。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplexityProfile {
    /// 合成分数 [0.0, 1.0]
    pub score: f64,
    /// 各维度原始分数
    pub dimensions: ComplexityDimensions,
    /// 预估输出字符数
    pub estimated_output_chars: u32,
    /// 是否包含决策点
    pub has_decisions: bool,
    /// 是否需要外部信息
    pub needs_external_info: bool,
    /// 涉及领域数
    pub domain_count: u32,
    /// 评分置信度（信号覆盖率）
    pub assessment_confidence: f64,
}

/// 7 维复杂度信号
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplexityDimensions {
    /// D1: 输入长度 [0, 1]
    pub input_length: f64,
    /// D2: 结构复杂度 [0, 1]（多步骤/条件/组合）
    pub structural: f64,
    /// D3: 领域交叉度 [0, 1]
    pub domain_crossing: f64,
    /// D4: 决策密度 [0, 1]
    pub decision_density: f64,
    /// D5: 输出规模 [0, 1]
    pub output_scale: f64,
    /// D6: 外部依赖度 [0, 1]
    pub external_dependency: f64,
    /// D7: 精确度要求 [0, 1]
    pub precision_requirement: f64,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 门控作用域
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 门控作用域 — 定义门控在 Agent Team 中的作用点
///
/// ## 核心原则
/// 门控跟随"人机边界"：只在 Agent → 用户的输出点生效。
/// Agent → Agent 的交互永远豁免。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[derive(Default)]
pub enum GateScope {
    /// 单 Agent 对话：每次输出都可能触发门控
    #[default]
    SingleAgent,
    /// Agent Team 执行前：Leader 汇总 → 用户确认
    TeamPreExecution,
    /// Agent Team 执行中：豁免，不中断
    TeamInExecution,
    /// Agent Team 执行后：Leader 汇总结果 → 用户审查
    TeamPostExecution,
}


// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 执行计划估算
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 执行计划估算 — 双单位系统
///
/// ## 场景
/// Leader 在 PreExecutionBrief 中输出。
/// Agent 视角（token/时间）用于内部调度。
/// 人类等效视角（人/天）用于用户理解价值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionEstimate {
    /// 确定性指标（跨模型稳定）
    pub deterministic: DeterministicEstimate,
    /// 模型相关指标（区间）
    pub model_dependent: ModelDependentEstimate,
    /// 人类等效视角
    pub human_equivalent: HumanEquivalent,
}

/// 确定性指标：不依赖 tokenizer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeterministicEstimate {
    /// LLM 调用次数
    pub llm_calls: u32,
    /// 预估输出字符数
    pub output_chars: u32,
    /// 预估耗时（秒）
    pub duration_secs: u32,
    /// 子任务数
    pub subtask_count: u32,
}

/// 模型相关指标：给区间
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDependentEstimate {
    /// Token 区间 [low, high]
    pub token_range: (u32, u32),
    /// 费用区间 [low, high] USD
    pub cost_range_usd: (f64, f64),
    /// 参考模型
    pub reference_model: String,
    /// 估算方法
    pub method: EstimateMethod,
}

/// 估算方法
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EstimateMethod {
    /// 基于当前模型
    ModelSpecific { model: String, confidence: f64 },
    /// 通用近似
    GenericApprox { chars_per_token_ratio: f64 },
    /// 基于历史
    HistoricalAvg { sample_size: u32, model: String },
}

/// 人类等效工时
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanEquivalent {
    /// 等效人天
    pub person_days: f64,
    /// 等效人时
    pub person_hours: f64,
    /// 假设角色
    pub assumed_role: String,
    /// 置信度
    pub confidence: EstimateConfidence,
}

/// 估算置信度
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EstimateConfidence {
    High,
    Medium,
    Low,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 事件
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 控制器发出的事件（L4 订阅渲染）
#[derive(Debug, Clone)]
pub enum ProgressiveEvent {
    /// 策略已决定
    StrategyDecided(OutputStrategy),
    /// 清单已生成
    ChecklistReady(Checklist),
    /// 等待用户确认
    AwaitingInput { timeout_secs: u64 },
    /// 段落开始
    SectionStarted { section_id: u32, title: String },
    /// 段落完成
    SectionCompleted { section_id: u32 },
    /// 全部完成
    OutputCompleted { total_tokens: u64 },
    /// 超时自动续写
    TimeoutAutoProceeded,
    /// 降级（LLM 未遵循格式）
    DegradedToStaged { reason: String },
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 执行后审查
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 执行偏差（Agent Team 执行后检测）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deviation {
    pub subtask_id: u32,
    pub expected: String,
    pub actual: String,
    pub severity: DeviationSeverity,
}

/// 偏差严重程度
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviationSeverity {
    /// 细节差异
    Low,
    /// 局部偏差
    Medium,
    /// 方向性偏离
    High,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_autonomy_thresholds() {
        assert_eq!(AutonomyLevel::High.to_thresholds(), (0.70, 0.90));
        assert_eq!(AutonomyLevel::Medium.to_thresholds(), (0.30, 0.50));
        assert_eq!(AutonomyLevel::Low.to_thresholds(), (0.30, 0.40));
    }

    #[test]
    fn test_autonomy_default() {
        assert_eq!(AutonomyLevel::default(), AutonomyLevel::Medium);
    }

    #[test]
    fn test_checklist_validity() {
        let empty = Checklist::placeholder();
        assert!(!empty.is_valid());

        let with_item = Checklist {
            info_items: vec![ChecklistItem {
                id: 1,
                category: ChecklistCategory::InfoAcquired,
                label: "test".into(),
                detail: None,
                source: None,
                response: None,
            }],
            decisions: Vec::new(),
            context_digest: "test".into(),
            timeout: None,
            blocking: true,
        };
        assert!(with_item.is_valid());
    }

    #[test]
    fn test_gate_scope_default() {
        assert_eq!(GateScope::default(), GateScope::SingleAgent);
    }

    #[test]
    fn test_deviation_ordering() {
        assert!(DeviationSeverity::Low < DeviationSeverity::Medium);
        assert!(DeviationSeverity::Medium < DeviationSeverity::High);
    }

    #[test]
    fn test_forced_gated_types_monotonic() {
        let high = AutonomyLevel::High.forced_gated_types().len();
        let medium = AutonomyLevel::Medium.forced_gated_types().len();
        let low = AutonomyLevel::Low.forced_gated_types().len();
        assert!(high <= medium);
        assert!(medium <= low);
    }
}
