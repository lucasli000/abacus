//! WorkflowEngine — 状态机驱动器 + 自适应调度
//!
//! ## 场景

// 历史 doc 注释经过多轮重构后存在孤立段（doc 块跟随的项被移走或重命名）；
// 整体重写注释拓扑超出本次 lint 清理范围。allow 此 lint 是显式选择。
#![allow(clippy::empty_line_after_doc_comments)]

//! 驱动 WorkflowPhase 状态机从 Comprehension 推进到 Completed/Failed。
//! 根据复杂度/场景/用户偏好动态调整阶段排布。
//!
//! ## 依赖
//! - `workflow_gate`: WorkflowPhase, PhaseConfig, WorkflowContext, PhaseChecker
//! - `task_analyzer`: TaskKind, ComplexityProfile
//! - `memory_palace::BehaviorPalace`: 用户偏好历史
//!
//! ## 引用关系
//! - 被 `CoreLoop::process_turn()` 在复杂度超阈值时创建
//! - 内部在 Phase 10 (Execution) 调用现有 TurnPipeline
//! - 通过 event_tx 广播 WorkflowEvent 给 TUI/日志
//!
//! ## 补强设计
//! 1. 置信度传播：前阶段低置信度 → 后阶段审查力度加重
//! 2. 阶段间 findings 注入：前阶段发现 → 聚焦后阶段审查方向
//! 3. 动态阈值校准：用户历史 accept/reject → 调整 pass 阈值
//! 4. 早停机制：连续零发现 → 后续阶段降为 spot-check
//! 5. 回退预算：max_rollbacks + 回退深度限制（防振荡）

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{broadcast, RwLock};

use super::workflow_gate::*;
use crate::core::task_analyzer::TaskKind;

// ─── Adaptive Scheduler（自适应调度器）─────────────────────────────────────

/// 用户偏好信号（从 BehaviorPalace 历史提取）
#[derive(Debug, Clone)]
pub struct UserPreferenceSignal {
    /// 用户对"快出结果"的偏好 [0, 1]（高 = 倾向跳过审查）
    pub speed_preference: f64,
    /// 用户对"严格审查"的偏好 [0, 1]（高 = 倾向全量审查）
    pub rigor_preference: f64,
    /// 历史各阶段 accept 率（phase_name → accept_rate）
    /// accept_rate < 0.3 的阶段说明用户不认可其价值 → 可降权
    pub phase_accept_rates: HashMap<String, f64>,
    /// 用户最近明确指令（"直接做"/"仔细看"等）
    pub explicit_directive: Option<AdaptiveDirective>,
}

/// 用户显式指令（从输入中检测）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdaptiveDirective {
    /// "直接做"/"按我说的执行" → 最小审查
    JustExecute,
    /// "仔细看"/"严格审查" → 最大审查
    Thorough,
    /// "快点"/"不用审查" → 跳过 post-execution
    SkipReview,
    /// 无特殊指令
    None,
}

impl Default for UserPreferenceSignal {
    fn default() -> Self {
        Self {
            speed_preference: 0.5,
            rigor_preference: 0.5,
            phase_accept_rates: HashMap::new(),
            explicit_directive: Some(AdaptiveDirective::None),
        }
    }
}

/// 自适应调度器 — 决定每个阶段的执行策略
///
/// ## 三维输入
/// 1. 方案复杂度（TaskAnalyzer 输出）
/// 2. 场景复杂度（领域 + 涉及范围 + 破坏性）
/// 3. 用户偏好（历史行为 + 显式指令）
///
/// ## 输出
/// 每个阶段的 PhaseSchedule（run/skip/spot_check + 调整后的阈值）
#[derive(Debug, Clone)]
pub struct AdaptiveScheduler {
    /// 基础模式（由复杂度决定）
    pub base_mode: WorkflowMode,
    /// 用户偏好信号
    pub preference: UserPreferenceSignal,
    /// 场景复杂度补正（破坏性操作加权、跨域任务加权）
    pub scenario_boost: f64,
    /// 每个阶段的调度决策缓存
    phase_schedules: HashMap<String, PhaseSchedule>,
    /// 置信度传播累积器
    confidence_chain: f64,
    /// 连续零发现计数（用于早停判断）
    zero_finding_streak: u32,
}

/// 单阶段调度决策
#[derive(Debug, Clone)]
pub struct PhaseSchedule {
    /// 执行策略
    pub action: PhaseAction,
    /// 调整后的通过阈值（越高越严格）
    pub adjusted_threshold: f64,
    /// 审查深度 [0, 1]（1.0 = 完全审查，0.3 = spot-check）
    pub depth: f64,
    /// 该阶段的审查焦点（从前阶段 findings 继承）
    pub focus_areas: Vec<String>,
}

/// 阶段执行策略
#[derive(Debug, Clone, PartialEq)]
pub enum PhaseAction {
    /// 正常执行
    Run,
    /// 跳过（复杂度不足/用户偏好/模式决定）
    Skip,
    /// 抽样检查（降级执行，只检查关键维度）
    SpotCheck,
}

impl AdaptiveScheduler {
    /// 从三维信号构建调度器
    pub fn new(
        complexity_score: f64,
        task_kind: &TaskKind,
        preference: UserPreferenceSignal,
        config: &PhaseConfig,
    ) -> Self {
        let base_mode = WorkflowMode::from_complexity(complexity_score, config);

        // 场景复杂度补正
        let scenario_boost = Self::compute_scenario_boost(task_kind, &preference);

        let mut scheduler = Self {
            base_mode,
            preference,
            scenario_boost,
            phase_schedules: HashMap::new(),
            confidence_chain: 1.0,
            zero_finding_streak: 0,
        };

        // 预计算所有阶段的调度
        scheduler.precompute_schedules(config);
        scheduler
    }

    /// 场景复杂度补正：某些场景需要额外审查力度
    fn compute_scenario_boost(task_kind: &TaskKind, pref: &UserPreferenceSignal) -> f64 {
        let mut boost: f64 = 0.0;

        // 代码修改/架构设计 → 补正
        match task_kind {
            TaskKind::Architecture => boost += 0.2,
            TaskKind::CodeWriting | TaskKind::FileEdit => boost += 0.1,
            TaskKind::Debugging => boost += 0.05,
            _ => {}
        }

        // 用户显式 Thorough 指令 → 大幅补正
        if pref.explicit_directive == Some(AdaptiveDirective::Thorough) {
            boost += 0.3;
        }

        // 用户 rigor_preference 高 → 补正
        if pref.rigor_preference > 0.7 {
            boost += 0.15;
        }

        boost.min(0.5_f64) // 最大补正 0.5
    }

    /// 预计算所有阶段的调度决策
    fn precompute_schedules(&mut self, _config: &PhaseConfig) {
        let all_phases = [
            "comprehension", "decomposition", "solution_design",
            "internal_review", "cross_reference_review", "cascade_impact",
            "roi_evaluation", "final_solution", "execution_planning",
            "execution", "multi_perspective_review", "progressive_layer_review",
            "adversarial_stress", "user_perception_check", "maintenance_assessment",
        ];

        for phase_name in all_phases {
            let phase = phase_from_name(phase_name);
            let schedule = self.compute_schedule_for(&phase);
            self.phase_schedules.insert(phase_name.to_string(), schedule);
        }
    }

    /// 为单个阶段计算调度决策
    fn compute_schedule_for(&self, phase: &WorkflowPhase) -> PhaseSchedule {
        // 1. 基础模式决定是否跳过
        if self.base_mode.should_skip(phase) && self.scenario_boost < 0.2 {
            return PhaseSchedule {
                action: PhaseAction::Skip,
                adjusted_threshold: 0.0,
                depth: 0.0,
                focus_areas: Vec::new(),
            };
        }

        // 2. 用户显式指令覆盖
        match &self.preference.explicit_directive {
            Some(AdaptiveDirective::JustExecute)
                // 只保留 Execution + Comprehension
                if phase.zone() != WorkflowZone::Execution
                    && *phase != WorkflowPhase::Comprehension
                    && *phase != WorkflowPhase::MultiPerspectiveReview
                => {
                    return PhaseSchedule {
                        action: PhaseAction::Skip,
                        adjusted_threshold: 0.0,
                        depth: 0.0,
                        focus_areas: Vec::new(),
                    };
                }
            Some(AdaptiveDirective::SkipReview)
                if phase.zone() == WorkflowZone::PostExecution
                    && *phase != WorkflowPhase::MultiPerspectiveReview
                => {
                    return PhaseSchedule {
                        action: PhaseAction::Skip,
                        adjusted_threshold: 0.0,
                        depth: 0.0,
                        focus_areas: Vec::new(),
                    };
                }
            _ => {}
        }

        // 3. 用户历史 accept_rate 低 → 降级为 spot-check
        let phase_name = phase.name();
        let accept_rate = self.preference.phase_accept_rates
            .get(phase_name)
            .copied()
            .unwrap_or(0.7); // 默认 70% 接受率

        let action = if accept_rate < 0.3 {
            PhaseAction::SpotCheck // 用户很少接受此阶段的 findings → 降级
        } else {
            PhaseAction::Run
        };

        // 4. 调整阈值（场景补正 + 用户 rigor）
        let base_threshold = 0.6;
        let adjusted = (base_threshold + self.scenario_boost * 0.3
            + self.preference.rigor_preference * 0.2).min(0.95);

        // 5. 审查深度
        let depth = if action == PhaseAction::SpotCheck {
            0.3
        } else if self.preference.rigor_preference > 0.8 {
            1.0
        } else {
            0.7 + self.scenario_boost * 0.3
        };

        PhaseSchedule {
            action,
            adjusted_threshold: adjusted,
            depth: depth.min(1.0),
            focus_areas: Vec::new(), // 运行时由 findings 注入填充
        }
    }

    /// 获取指定阶段的调度决策
    pub fn schedule_for(&self, phase: &WorkflowPhase) -> PhaseSchedule {
        self.phase_schedules
            .get(phase.name())
            .cloned()
            .unwrap_or(PhaseSchedule {
                action: PhaseAction::Run,
                adjusted_threshold: 0.6,
                depth: 0.7,
                focus_areas: Vec::new(),
            })
    }

    /// 更新置信度链（每阶段完成后调用）
    ///
    /// ## 置信度传播规则
    /// - 前阶段 findings 多 → confidence 降低 → 后阶段审查加重
    /// - 前阶段零发现 → confidence 维持 → 触发早停判断
    pub fn update_confidence(&mut self, findings_count: usize, phase: &WorkflowPhase) {
        if findings_count == 0 {
            self.zero_finding_streak += 1;
            // 连续 3 个阶段零发现 → 后续阶段可降级为 spot-check
            if self.zero_finding_streak >= 3 {
                self.downgrade_remaining(phase);
            }
        } else {
            self.zero_finding_streak = 0;
            // findings 越多 → confidence 越低 → 后续阶段阈值越高
            let penalty = (findings_count as f64 * 0.1).min(0.4);
            self.confidence_chain = (self.confidence_chain - penalty).max(0.3);
            // 提升后续阶段的 threshold
            self.boost_remaining(phase);
        }
    }

    /// 注入焦点区域（前阶段 findings → 后阶段 focus_areas）
    pub fn inject_focus(&mut self, phase: &WorkflowPhase, focus: Vec<String>) {
        if let Some(next) = phase.next() {
            if let Some(schedule) = self.phase_schedules.get_mut(next.name()) {
                schedule.focus_areas.extend(focus);
            }
        }
    }

    /// 连续零发现 → 后续阶段降级为 spot-check（早停机制）
    fn downgrade_remaining(&mut self, current: &WorkflowPhase) {
        let current_ord = current.ordinal();
        for (name, schedule) in &mut self.phase_schedules {
            let phase = phase_from_name(name);
            if phase.ordinal() > current_ord && schedule.action == PhaseAction::Run {
                schedule.action = PhaseAction::SpotCheck;
                schedule.depth = 0.3;
            }
        }
    }

    /// findings 多 → 后续阶段审查力度加重
    fn boost_remaining(&mut self, current: &WorkflowPhase) {
        let current_ord = current.ordinal();
        let boost = 0.1;
        for (name, schedule) in &mut self.phase_schedules {
            let phase = phase_from_name(name);
            if phase.ordinal() > current_ord {
                schedule.adjusted_threshold = (schedule.adjusted_threshold + boost).min(0.95);
                schedule.depth = (schedule.depth + 0.1).min(1.0);
            }
        }
    }

    /// 当前置信度
    pub fn confidence(&self) -> f64 {
        self.confidence_chain
    }
}

/// 大输出语义分段：按自然边界（代码块/heading/空行）切分
///
/// ## 分段规则
/// 1. 代码标记（```...```）→ 整块为一段（type=code）
/// 2. Markdown heading (## / ###) → 按 heading 切分
/// 3. 双空行分隔 → 按段落切分
/// 4. 以上都不匹配 → 按 4KB 字符切分
fn segment_output(output: &str) -> Vec<super::workflow_gate::OutputSegment> {
    use super::workflow_gate::OutputSegment;

    let mut segments = Vec::new();
    let mut current_content = String::new();
    let mut current_offset = 0usize;
    let mut char_offset = 0usize;
    let mut in_code_block = false;
    let mut segment_type = "text";

    for line in output.lines() {
        let line_chars = line.chars().count() + 1; // +1 for \n

        if line.starts_with("```") {
            if in_code_block {
                // 代码块结束
                current_content.push_str(line);
                current_content.push('\n');
                segments.push(OutputSegment {
                    index: segments.len(),
                    content: std::mem::take(&mut current_content),
                    segment_type: "code".into(),
                    offset: current_offset,
                });
                current_offset = char_offset + line_chars;
                in_code_block = false;
                segment_type = "text";
            } else {
                // 代码块开始：先保存之前的文本段
                if !current_content.trim().is_empty() {
                    segments.push(OutputSegment {
                        index: segments.len(),
                        content: std::mem::take(&mut current_content),
                        segment_type: segment_type.into(),
                        offset: current_offset,
                    });
                    current_offset = char_offset;
                } else {
                    current_content.clear();
                    current_offset = char_offset;
                }
                current_content.push_str(line);
                current_content.push('\n');
                in_code_block = true;
                segment_type = "code";
            }
        } else if !in_code_block && (line.starts_with("## ") || line.starts_with("### ")) {
            // Heading 边界：上一段结束
            if !current_content.trim().is_empty() {
                segments.push(OutputSegment {
                    index: segments.len(),
                    content: std::mem::take(&mut current_content),
                    segment_type: segment_type.into(),
                    offset: current_offset,
                });
                current_offset = char_offset;
            } else {
                current_content.clear();
                current_offset = char_offset;
            }
            current_content.push_str(line);
            current_content.push('\n');
            segment_type = "text";
        } else {
            current_content.push_str(line);
            current_content.push('\n');

            // 非代码块中，当前段超过 4KB → 在下一个空行处切分
            if !in_code_block && current_content.len() > 4096 && line.trim().is_empty() {
                segments.push(OutputSegment {
                    index: segments.len(),
                    content: std::mem::take(&mut current_content),
                    segment_type: segment_type.into(),
                    offset: current_offset,
                });
                current_offset = char_offset + line_chars;
            }
        }

        char_offset += line_chars;
    }

    // 最后一段
    if !current_content.trim().is_empty() {
        segments.push(OutputSegment {
            index: segments.len(),
            content: current_content,
            segment_type: segment_type.into(),
            offset: current_offset,
        });
    }

    segments
}

/// 从 name 还原 WorkflowPhase（用于内部索引）
fn phase_from_name(name: &str) -> WorkflowPhase {
    match name {
        "comprehension" => WorkflowPhase::Comprehension,
        "decomposition" => WorkflowPhase::Decomposition,
        "solution_design" => WorkflowPhase::SolutionDesign,
        "internal_review" => WorkflowPhase::InternalReview,
        "cross_reference_review" => WorkflowPhase::CrossReferenceReview,
        "cascade_impact" => WorkflowPhase::CascadeImpact,
        "roi_evaluation" => WorkflowPhase::RoiEvaluation,
        "final_solution" => WorkflowPhase::FinalSolution,
        "execution_planning" => WorkflowPhase::ExecutionPlanning,
        "execution" => WorkflowPhase::Execution,
        "multi_perspective_review" => WorkflowPhase::MultiPerspectiveReview,
        "progressive_layer_review" => WorkflowPhase::ProgressiveLayerReview,
        "adversarial_stress" => WorkflowPhase::AdversarialStress,
        "user_perception_check" => WorkflowPhase::UserPerceptionCheck,
        "maintenance_assessment" => WorkflowPhase::MaintenanceAssessment,
        _ => WorkflowPhase::Comprehension, // fallback
    }
}

// ─── WorkflowEngine 驱动器 ────────────────────────────────────────────────

/// 工作流执行结果
#[derive(Debug, Clone)]
pub struct WorkflowResult {
    pub phase: WorkflowPhase,
    pub context: WorkflowContext,
    pub total_duration_ms: u64,
    pub phases_executed: usize,
    pub rollback_count: u32,
}

/// 工作流引擎 — 状态机驱动器
///
/// ## 生命周期
/// 每次 process_turn() 触发复杂任务时创建，执行完即销毁。
/// 不跨 turn 复用。
///
/// ## 执行模型
/// ```text
/// loop {
///   current_phase → scheduler.schedule_for()
///     → Skip: advance
///     → SpotCheck: run checker with depth=0.3
///     → Run: run checker with full depth
///   verdict:
///     → Pass: advance + update_confidence
///     → Rollback: go back (budget check)
///     → Fail: terminal
/// }
/// ```
// ─── 工作流审计日志 ─────────────────────────────────────────────────

/// 工作流审计日志条目
///
/// ## 格式
/// 每条目序列化为一行 JSON（JSON Lines），追加到当前工作目录下的
/// `workflow_audit.jsonl`。可直接用 `jq` 查询决策链：
///   `jq 'select(.event.PhaseCompleted.phase == "validation")' workflow_audit.jsonl`
///
/// ## 引用关系
/// - 被 `WorkflowAuditLog::record()` 创建并写入
/// - 被 `WorkflowEngine::emit_event()` 调用
use serde::{Serialize, Deserialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowAuditEntry {
    pub session_id: String,
    pub timestamp_ms: i64,
    /// 工作流事件（序列化为 JSON 嵌入）
    pub event: WorkflowEvent,
}

/// 轻量审计日志 — 追加写入 JSONL 文件
///
/// ## 线程安全
/// `record()` 使用文件系统废层追加（O_APPEND）。
/// 单条目 < 4 KB，单次 write syscall 原子，无锁竞争。
///
/// ## 生命周期
/// - 创建：`WorkflowEngine::with_session()` 构造器
/// - 追加：`emit_event()` 内展开调用 `record()`
/// - 无销毁动作：JSONL 文件由外部策略管理
#[derive(Debug)]
pub struct WorkflowAuditLog {
    path: std::path::PathBuf,
    session_id: String,
}

impl WorkflowAuditLog {
    /// 创建审计日志（目录不存在时自动创建）
    pub fn new(path: std::path::PathBuf, session_id: impl Into<String>) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self { path, session_id: session_id.into() })
    }

    /// 追加一条审计记录（失败时静默，不影响主流程）
    pub fn record(&self, event: &WorkflowEvent) {
        let entry = WorkflowAuditEntry {
            session_id: self.session_id.clone(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            event: event.clone(),
        };
        if let Ok(mut line) = serde_json::to_string(&entry) {
            line.push('\n');
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true).append(true).open(&self.path)
            {
                let _ = file.write_all(line.as_bytes());
            }
        }
    }
}

pub struct WorkflowEngine {
    phase: RwLock<WorkflowPhase>,
    config: PhaseConfig,
    scheduler: RwLock<AdaptiveScheduler>,
    ctx: RwLock<WorkflowContext>,
    checkers: HashMap<String, Arc<dyn PhaseChecker>>,
    event_tx: broadcast::Sender<WorkflowEvent>,
    /// 工作流审计日志（可选 — 未注入时仅广播不持久化）
    ///
    /// ## 生命周期
    /// - 创建：`WorkflowEngine::with_session()` 将其注入
    /// - 消费：`emit_event()` 内调用 `audit_log.record()`
    /// - 销毁：随 `WorkflowEngine` Drop
    audit_log: Option<Arc<WorkflowAuditLog>>,
}

impl WorkflowEngine {
    /// 创建引擎实例
    ///
    /// ## 参数
    /// - `input`: 用户原始输入
    /// - `task_kind`: 任务分类
    /// - `complexity_score`: 复杂度评分
    /// - `preference`: 用户偏好信号
    /// - `config`: 阶段配置
    pub fn new(
        input: String,
        task_kind: TaskKind,
        complexity_score: f64,
        preference: UserPreferenceSignal,
        config: PhaseConfig,
    ) -> Self {
        let mode = WorkflowMode::from_complexity(
            complexity_score + preference.rigor_preference * 0.1, // rigor 微调
            &config,
        );

        let scheduler = AdaptiveScheduler::new(
            complexity_score, &task_kind, preference, &config,
        );

        let ctx = WorkflowContext::new(input, task_kind, complexity_score, mode.clone());

        let (event_tx, _) = broadcast::channel(32);

        Self {
            phase: RwLock::new(WorkflowPhase::Comprehension),
            config,
            scheduler: RwLock::new(scheduler),
            ctx: RwLock::new(ctx),
            checkers: HashMap::new(),
            event_tx,
            audit_log: None,
        }
    }

    /// 创建引擎并注入审计日志。
    ///
    /// ## 参数
    /// - `session_id`: 当前 session 标识（写入每条审计记录）
    /// - `audit_path`: JSONL 审计日志路径（推荐 `~/.abacus/workflow_audit.jsonl`）
    ///
    /// ## 生命周期
    /// - 审计日志随 `WorkflowEngine` Drop，JSONL 文件持续存在
    pub fn with_session(
        input: String,
        task_kind: crate::core::task_analyzer::TaskKind,
        complexity_score: f64,
        preference: UserPreferenceSignal,
        config: PhaseConfig,
        session_id: impl Into<String>,
        audit_path: std::path::PathBuf,
    ) -> Self {
        let mut engine = Self::new(input, task_kind, complexity_score, preference, config);
        let session_id = session_id.into();
        match WorkflowAuditLog::new(audit_path, &session_id) {
            Ok(log) => { engine.audit_log = Some(Arc::new(log)); }
            Err(e) => { tracing::warn!("工作流审计日志初始化失败（仅广播不持久化）: {e}"); }
        }
        engine
    }

    /// 发送工作流事件：广播到所有订阅者 + 可选审计持久化。
    ///
    /// ## 审计行为
    /// 若 `audit_log` 已注入，每个事件均追加写入 JSONL。
    /// 广播失败（无订阅者）是预期行为，静默忽略。
    fn emit_event(&self, event: WorkflowEvent) {
        if let Some(ref log) = self.audit_log {
            log.record(&event);
        }
        let _ = self.event_tx.send(event);
    }

    /// 注册阶段审查器
    pub fn register_checker(&mut self, checker: Arc<dyn PhaseChecker>) {
        let name = checker.phase_name().to_string();
        self.checkers.insert(name, checker);
    }

    /// 获取事件订阅
    pub fn subscribe(&self) -> broadcast::Receiver<WorkflowEvent> {
        self.event_tx.subscribe()
    }

    /// 驱动状态机：循环执行 check → transition → 直到终态
    ///
    /// ## 执行流程
    /// 1. 检查当前阶段的 schedule（Run/Skip/SpotCheck）
    /// 2. Skip → advance to next
    /// 3. Run/SpotCheck → 执行对应 checker
    /// 4. 根据 verdict 决定 advance/rollback/fail
    /// 5. 更新 confidence chain + inject focus
    /// 6. 循环直到 terminal
    pub async fn drive(&self) -> WorkflowResult {
        let start = Instant::now();

        // Fast-path 检测
        {
            let scheduler = self.scheduler.read().await;
            if scheduler.base_mode == WorkflowMode::FastPath {
                *self.phase.write().await = WorkflowPhase::FastPathCompleted;
                self.emit_event(WorkflowEvent::FastPathActivated {
                    reason: "complexity below threshold".into(),
                });
                return WorkflowResult {
                    phase: WorkflowPhase::FastPathCompleted,
                    context: self.ctx.read().await.clone(),
                    total_duration_ms: start.elapsed().as_millis() as u64,
                    phases_executed: 0,
                    rollback_count: 0,
                };
            }
        }

        let mut phases_executed = 0u32;

        loop {
            let current = self.phase.read().await.clone();
            if current.is_terminal() {
                break;
            }

            // 超时保护（>= 使得 max_total_duration_secs=0 立即触发）
            if start.elapsed().as_secs() >= self.config.max_total_duration_secs {
                let failed = WorkflowPhase::Failed {
                    phase: current.name().into(),
                    reason: format!("workflow timeout after {}s", self.config.max_total_duration_secs),
                };
                *self.phase.write().await = failed;
                break;
            }

            // 获取调度决策
            let schedule = {
                let scheduler = self.scheduler.read().await;
                scheduler.schedule_for(&current)
            };

            self.emit_event(WorkflowEvent::PhaseEntered {
                phase: current.name().into(),
                zone: format!("{:?}", current.zone()),
                timestamp: chrono::Utc::now().timestamp(),
            });

            let phase_start = Instant::now();

            // 根据 action 决定执行方式
            let verdict = match schedule.action {
                PhaseAction::Skip => PhaseVerdict::Skip,
                PhaseAction::Run | PhaseAction::SpotCheck => {
                    self.execute_checker(&current, &schedule).await
                }
            };

            let duration_ms = phase_start.elapsed().as_millis() as u64;
            let findings_count = match &verdict {
                PhaseVerdict::Pass { findings } => findings.len(),
                PhaseVerdict::Rollback { .. } => 1,
                PhaseVerdict::Fail { .. } => 1,
                PhaseVerdict::Skip => 0,
            };

            // 记录历史
            {
                let mut ctx = self.ctx.write().await;
                ctx.phase_history.push(PhaseHistoryEntry {
                    phase: current.name().into(),
                    verdict: format!("{:?}", verdict),
                    duration_ms,
                    findings_count,
                    timestamp: chrono::Utc::now().timestamp(),
                });
            }

            self.emit_event(WorkflowEvent::PhaseCompleted {
                phase: current.name().into(),
                verdict: format!("{:?}", verdict),
                duration_ms,
                findings_count,
            });

            phases_executed += 1;

            // 更新自适应调度（置信度传播 + 焦点注入）
            {
                let mut scheduler = self.scheduler.write().await;
                scheduler.update_confidence(findings_count, &current);
                if let PhaseVerdict::Pass { ref findings } = verdict {
                    if !findings.is_empty() {
                        scheduler.inject_focus(&current, findings.clone());
                    }
                }
            }

            // 根据 verdict 转换状态
            match verdict {
                PhaseVerdict::Pass { .. } | PhaseVerdict::Skip => {
                    self.advance(&current).await;
                }
                PhaseVerdict::Rollback { to_phase, reason } => {
                    let rollback_count = self.ctx.read().await.rollback_count;
                    if rollback_count >= self.config.max_rollbacks {
                        let failed = WorkflowPhase::Failed {
                            phase: current.name().into(),
                            reason: format!("max rollbacks ({}) exceeded: {}", self.config.max_rollbacks, reason),
                        };
                        *self.phase.write().await = failed;
                        break;
                    }
                    let target = phase_from_name(&to_phase);
                    // 验证回退转换合法性（防止审查器返回非法回退目标）
                    if !current.can_transition_to(&target) {
                        tracing::warn!(
                            "illegal rollback {:?} → {:?}, falling back to SolutionDesign",
                            current, target
                        );
                        // 强制回退到 SolutionDesign（安全默认值）
                        *self.phase.write().await = WorkflowPhase::SolutionDesign;
                    } else {
                        *self.phase.write().await = target;
                    }
                    self.emit_event(WorkflowEvent::RollbackTriggered {
                        from: current.name().into(),
                        to: to_phase.clone(),
                        reason: reason.clone(),
                    });
                    self.ctx.write().await.rollback_count += 1;
                }
                PhaseVerdict::Fail { reason } => {
                    let failed = WorkflowPhase::Failed {
                        phase: current.name().into(),
                        reason: reason.clone(),
                    };
                    self.emit_event(WorkflowEvent::WorkflowFailed {
                        at_phase: current.name().into(),
                        reason,
                    });
                    *self.phase.write().await = failed;
                    break;
                }
            }
        }

        let final_phase = self.phase.read().await.clone();
        let ctx = self.ctx.read().await.clone();
        let total_duration = start.elapsed().as_millis() as u64;

        if matches!(final_phase, WorkflowPhase::Completed { .. } | WorkflowPhase::FastPathCompleted) {
            self.emit_event(WorkflowEvent::WorkflowCompleted {
                total_phases: phases_executed as usize,
                rollbacks: ctx.rollback_count,
                duration_ms: total_duration,
                mode: ctx.mode.clone(),
            });
        }

        WorkflowResult {
            phase: final_phase,
            context: ctx.clone(),
            total_duration_ms: total_duration,
            phases_executed: phases_executed as usize,
            rollback_count: ctx.rollback_count,
        }
    }

    /// 执行阶段审查器
    async fn execute_checker(&self, phase: &WorkflowPhase, _schedule: &PhaseSchedule) -> PhaseVerdict {
        let checker = match self.checkers.get(phase.name()) {
            Some(c) => c.clone(),
            None => {
                // 无审查器注册 → 默认通过（骨架模式）
                return PhaseVerdict::Pass {
                    findings: vec![format!("no checker registered for phase: {}", phase.name())],
                };
            }
        };

        let ctx = self.ctx.read().await.clone();
        checker.check(&ctx).await
    }

    /// 推进到下一阶段
    async fn advance(&self, current: &WorkflowPhase) {
        let next = match current.next() {
            Some(n) => n,
            None => {
                // MaintenanceAssessment 之后 → Completed
                let ctx = self.ctx.read().await;
                WorkflowPhase::Completed {
                    summary: format!(
                        "全流程完成: {} 阶段, {} 回退, 置信度 {:.0}%",
                        ctx.phase_history.len(),
                        ctx.rollback_count,
                        self.scheduler.read().await.confidence() * 100.0
                    ),
                }
            }
        };

        // 检查 Lite 模式的跳跃路径
        let scheduler = self.scheduler.read().await;
        let should_skip_next = scheduler.schedule_for(&next).action == PhaseAction::Skip;
        drop(scheduler);

        if should_skip_next && next.next().is_some() {
            // 跳过此阶段，直接到下下个
            *self.phase.write().await = next.clone();
            // drive() 循环下一轮会处理 skip verdict
        } else {
            *self.phase.write().await = next;
        }
    }

    /// 获取当前阶段
    pub async fn current_phase(&self) -> WorkflowPhase {
        self.phase.read().await.clone()
    }

    /// 获取进度 [0, 1]
    pub async fn progress(&self) -> f64 {
        self.ctx.read().await.progress()
    }

    /// 获取上下文快照（只读）
    pub async fn context_snapshot(&self) -> WorkflowContext {
        self.ctx.read().await.clone()
    }

    /// 外部写入上下文（Phase 10 Execution 完成后注入输出）
    ///
    /// ## 分段策略（大输出审查）
    /// 当输出超过 32KB 时，不截断丢弃，而是：
    /// 1. `execution_output` 存储完整输出（审查器可全量访问）
    /// 2. `output_segments` 将大输出按语义边界分段，供分段审查
    /// 3. Phase 11-15 的审查器可选择：全量扫描 or 分段逐块审查
    ///
    /// 性能保护在 clone 层面通过 Arc 共享实现（大输出只存一份）。
    pub async fn inject_execution_output(&self, output: String, tool_outputs: Vec<serde_json::Value>) {
        let mut ctx = self.ctx.write().await;

        // 大输出分段（供审查器分块检查，避免单次处理过大文本）
        if output.len() > Self::SEGMENT_THRESHOLD {
            ctx.output_segments = segment_output(&output);
        }

        ctx.execution_output = Some(output);

        // tool_outputs 保留最近 20 个（防止 clone 时 JSON 深拷贝过多）
        ctx.tool_outputs = if tool_outputs.len() > 20 {
            tool_outputs.into_iter().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect()
        } else {
            tool_outputs
        };
    }

    /// 分段阈值（超过此值触发语义分段）
    const SEGMENT_THRESHOLD: usize = 32_768;
}

// ─── Directive Detection ──────────────────────────────────────────────────

/// 从用户输入中检测显式指令
pub fn detect_directive(input: &str) -> AdaptiveDirective {
    let lower = input.to_lowercase();

    // "直接做"/"按我说的执行"/"just do it"
    let just_execute_patterns = [
        "直接做", "按我说的", "不用分析", "别废话", "just do it",
        "直接执行", "不需要审查", "skip review",
    ];
    if just_execute_patterns.iter().any(|p| lower.contains(p)) {
        return AdaptiveDirective::JustExecute;
    }

    // "仔细看"/"严格审查"
    let thorough_patterns = [
        "仔细", "严格审查", "全面检查", "thoroughly", "careful",
        "详细分析", "深入审查", "不能出错",
    ];
    if thorough_patterns.iter().any(|p| lower.contains(p)) {
        return AdaptiveDirective::Thorough;
    }

    // "快点"/"不用审查后面的"
    let skip_patterns = [
        "快点", "不用审查", "跳过检查", "skip check", "no review",
    ];
    if skip_patterns.iter().any(|p| lower.contains(p)) {
        return AdaptiveDirective::SkipReview;
    }

    AdaptiveDirective::None
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adaptive_scheduler_fast_path() {
        let pref = UserPreferenceSignal::default();
        let config = PhaseConfig::default();
        let scheduler = AdaptiveScheduler::new(0.1, &TaskKind::GeneralChat, pref, &config);
        assert_eq!(scheduler.base_mode, WorkflowMode::FastPath);
    }

    #[test]
    fn test_adaptive_scheduler_full() {
        let pref = UserPreferenceSignal {
            rigor_preference: 0.9,
            ..Default::default()
        };
        let config = PhaseConfig::default();
        let scheduler = AdaptiveScheduler::new(0.8, &TaskKind::Architecture, pref, &config);
        assert_eq!(scheduler.base_mode, WorkflowMode::Full);
        // Architecture 补正
        assert!(scheduler.scenario_boost > 0.1);
    }

    #[test]
    fn test_directive_detection() {
        assert_eq!(detect_directive("直接做吧"), AdaptiveDirective::JustExecute);
        assert_eq!(detect_directive("仔细看看这个方案"), AdaptiveDirective::Thorough);
        assert_eq!(detect_directive("快点完成"), AdaptiveDirective::SkipReview);
        assert_eq!(detect_directive("帮我分析一下"), AdaptiveDirective::None);
    }

    #[test]
    fn test_confidence_propagation() {
        let pref = UserPreferenceSignal::default();
        let config = PhaseConfig::default();
        let mut scheduler = AdaptiveScheduler::new(0.7, &TaskKind::CodeWriting, pref, &config);

        // 前 3 个阶段零发现 → 后续降级
        scheduler.update_confidence(0, &WorkflowPhase::Comprehension);
        scheduler.update_confidence(0, &WorkflowPhase::Decomposition);
        scheduler.update_confidence(0, &WorkflowPhase::SolutionDesign);

        // 连续 3 个零发现 → 后续阶段应降级为 SpotCheck
        let schedule = scheduler.schedule_for(&WorkflowPhase::AdversarialStress);
        assert_eq!(schedule.action, PhaseAction::SpotCheck);
    }

    #[test]
    fn test_findings_boost() {
        let pref = UserPreferenceSignal::default();
        let config = PhaseConfig::default();
        let mut scheduler = AdaptiveScheduler::new(0.7, &TaskKind::CodeWriting, pref, &config);

        let before = scheduler.schedule_for(&WorkflowPhase::Execution).adjusted_threshold;
        // 前阶段发现 5 个 findings → 后续阶段阈值应提高
        scheduler.update_confidence(5, &WorkflowPhase::InternalReview);
        let after = scheduler.schedule_for(&WorkflowPhase::Execution).adjusted_threshold;

        assert!(after > before, "findings should boost subsequent thresholds");
    }

    #[test]
    fn test_user_low_accept_rate_downgrades() {
        let mut rates = HashMap::new();
        rates.insert("adversarial_stress".to_string(), 0.2); // 用户很少接受此阶段建议
        let pref = UserPreferenceSignal {
            phase_accept_rates: rates,
            ..Default::default()
        };
        let config = PhaseConfig::default();
        let scheduler = AdaptiveScheduler::new(0.7, &TaskKind::CodeWriting, pref, &config);

        let schedule = scheduler.schedule_for(&WorkflowPhase::AdversarialStress);
        assert_eq!(schedule.action, PhaseAction::SpotCheck);
    }

    #[tokio::test]
    async fn test_engine_fast_path() {
        let engine = WorkflowEngine::new(
            "fix typo".into(),
            TaskKind::FileEdit,
            0.1, // 低复杂度
            UserPreferenceSignal::default(),
            PhaseConfig::default(),
        );
        let result = engine.drive().await;
        assert_eq!(result.phase, WorkflowPhase::FastPathCompleted);
        assert_eq!(result.phases_executed, 0);
    }

    #[tokio::test]
    async fn test_engine_no_checkers_passes_through() {
        let engine = WorkflowEngine::new(
            "complex refactoring task".into(),
            TaskKind::Architecture,
            0.8,
            UserPreferenceSignal {
                rigor_preference: 0.9,
                ..Default::default()
            },
            PhaseConfig { max_total_duration_secs: 10, ..Default::default() },
        );
        let result = engine.drive().await;
        // 无 checkers 注册 → 全部 Pass → 最终 Completed
        assert!(matches!(result.phase, WorkflowPhase::Completed { .. }));
        assert!(result.phases_executed > 0);
    }

    #[tokio::test]
    async fn test_engine_timeout() {
        // max_total_duration_secs=0 + >=检查 → 第一个阶段执行后即超时
        let engine = WorkflowEngine::new(
            "task".into(),
            TaskKind::CodeWriting,
            0.8,
            UserPreferenceSignal::default(),
            PhaseConfig { max_total_duration_secs: 0, ..Default::default() },
        );
        let result = engine.drive().await;
        // 0 秒超时：第一个阶段(Comprehension)会通过(无checker)，
        // 第二个阶段开始前检测到超时
        assert!(
            matches!(result.phase, WorkflowPhase::Failed { .. })
            || result.phases_executed <= 1,
            "should fail or only execute 1 phase, got {:?} with {} phases",
            result.phase, result.phases_executed
        );
    }
}
