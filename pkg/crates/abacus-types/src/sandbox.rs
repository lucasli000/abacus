//! Sandbox Task Engine — 任务沙箱核心类型
//!
//! ## 场景
//! 全托管模式下，任务拆解为原子步骤，每步在独立沙箱中执行：
//! - 独立 session、独立工具白名单、独立模型分配
//! - 每步有验收标准，通过后自动进入下一步
//! - 执行用贵模型，校验用便宜模型
//!
//! ## 引用关系
//! - 被 `abacus-core::sandbox` 引用（执行引擎）
//! - 被 `DeductionEngine` 引用（推演监控）
//! - 被 `abacus-cli` 引用（状态展示）

use serde::{Deserialize, Serialize};

// ─── 模型分配 ───────────────────────────────────────────────────────────────

/// 模型分配策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModelAssignment {
    /// 使用执行模型（贵）
    Execute,
    /// 使用校验模型（便宜）
    Verify,
    /// 显式指定 provider:model
    Fixed { provider: String, model: String },
}

// ─── 验收标准 ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Criterion {
    pub kind: CriterionKind,
    /// 通过阈值 (0.0 ~ 1.0)
    pub threshold: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CriterionKind {
    /// 编译通过
    Compiles,
    /// 测试全部通过
    TestsPass,
    /// 无 P0/P1 级别问题
    NoCriticalFindings,
    /// 代码覆盖率达到阈值
    Coverage { min_pct: f64 },
    /// 输出包含指定关键词
    KeywordMatch { required: Vec<String> },
    /// 自定义校验
    Custom { description: String },
}

// ─── 步骤状态机 ─────────────────────────────────────────────────────────────

/// 步骤执行状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StepState {
    Pending,
    Running,
    Verifying,
    Passed,
    Failed,
    MaxRetriesExceeded,
    Skipped,
}

/// 阶段状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PhaseState {
    Pending,
    Active { step_index: usize },
    Completed,
    Failed { step_id: String, reason: String },
}

/// 任务状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TaskState {
    Running,
    Completed,
    Failed,
}

// ─── 事件 ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxEvent {
    pub kind: SandboxEventKind,
    pub phase_id: String,
    pub step_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SandboxEventKind {
    StepStarted { model: String },
    StepCompleted,
    StepFailed { will_retry: bool, retries_left: u32 },
    VerificationPassed,
    VerificationFailed { criteria_results: Vec<String> },
    PhaseCompleted,
    TaskCompleted,
}

// ─── 沙箱步骤规格 ────────────────────────────────────────────────────────────

/// 原子步骤定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepSpec {
    pub id: String,
    pub description: String,
    /// 模型分配（执行/校验/固定）
    pub model: ModelAssignment,
    /// 主 Skill 名称（匹配 SkillEngine 中的 SkillDef，如 "futuapi"、"lark-doc"）
    pub skill: Option<String>,
    /// 工具白名单
    pub tools: Vec<String>,
    /// 输入引用：上一步的 output key 列表
    pub input_refs: Vec<String>,
    /// 验收标准
    pub accept_criteria: Vec<Criterion>,
    /// 最大重试次数
    pub max_retries: u32,
    /// 超时秒数
    pub timeout_secs: u64,
}

/// 阶段定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseSpec {
    pub id: String,
    pub description: String,
    pub steps: Vec<StepSpec>,
}

/// 完整任务定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub goal: String,
    pub phases: Vec<PhaseSpec>,
}

// ─── 沙箱运行配置 ───────────────────────────────────────────────────────────

/// 沙箱引擎配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// 执行模型（贵）
    pub execute_model: ModelAssignment,
    /// 校验模型（便宜）
    pub verify_model: ModelAssignment,
    /// 沙箱工作目录
    pub work_dir: String,
    /// 默认最大重试
    pub max_retries_per_step: u32,
    /// 默认超时
    pub default_timeout_secs: u64,
    /// 全局工具白名单（每步在此基础上取交集）
    pub global_tool_whitelist: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            execute_model: ModelAssignment::Fixed { provider: "deepseek".into(), model: "deepseek-v4-flash".into() },
            verify_model: ModelAssignment::Fixed { provider: "deepseek".into(), model: "deepseek-chat".into() },
            work_dir: "/tmp/abacus-sandbox".into(),
            max_retries_per_step: 2,
            default_timeout_secs: 120,
            global_tool_whitelist: vec![
                "fs_read".into(),
                "fs_info".into(),
                "fs_cwd".into(),
            ],
        }
    }
}
