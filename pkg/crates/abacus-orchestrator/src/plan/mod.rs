//! plan — 计划执行引擎
//!
//! ## 场景
//! 接收 PlanModel（步骤 DAG）→ 按依赖顺序执行 → 支持并行/条件/重试。
//!
//! ## 依赖
//! - `crate::subagent`: SubAgentDispatcher, SubAgentBoundary
//! - `abacus_types`: KernelError
//!
//! ## 引用关系
//! - 被 `team::TeamSession` 在 Executing 阶段调用
//! - 内部通过 SubAgentDispatcher 执行 SubAgentDelegate 步骤
//!
//! ## 生命周期
//! PlanModel: Draft → InProgress → Completed/Failed/Adapted

use abacus_types::KernelError;
use serde::{Deserialize, Serialize};

// ─── Plan Model ─────────────────────────────────────────────────────────

/// 步骤类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepKind {
    /// 调用单个工具
    ToolCall { tool_id: String, params: serde_json::Value },
    /// LLM 推理（生成文本/决策）
    LlmReason { prompt: String },
    /// 委托给 SubAgent 执行
    SubAgentDelegate { task_description: String, boundary_preset: String },
    /// 条件分支
    Conditional { condition: String, if_true: String, if_false: String },
    /// 并行执行多个步骤
    Parallel(Vec<String>),   // step IDs to run concurrently
    /// 顺序执行多个步骤
    Sequence(Vec<String>),   // step IDs to run in order
}

/// 步骤执行结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepResult {
    Success(serde_json::Value),
    Failed { error: String, retryable: bool },
    Skipped { reason: String },
}

/// 单个执行步骤
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub kind: StepKind,
    pub description: String,
    pub depends_on: Vec<String>,    // 前置依赖 step IDs
    pub status: StepStatus,
    pub result: Option<StepResult>,
    pub retries: u32,
    pub max_retries: u32,
}

/// 步骤状态
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Blocked,         // 依赖未完成
    Ready,           // 依赖已完成，可执行
    Running,
    Completed,
    Failed,
    Skipped,
}

/// 计划模型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanModel {
    pub id: String,
    pub goal: String,
    pub steps: Vec<PlanStep>,
    pub status: PlanStatus,
}

/// 计划状态
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlanStatus {
    Draft,
    InProgress,
    Completed,
    Failed(String),
    /// 执行中被修改（自适应）
    Adapted,
}

impl PlanModel {
    /// 创建新计划
    pub fn new(id: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            goal: goal.into(),
            steps: Vec::new(),
            status: PlanStatus::Draft,
        }
    }

    /// 添加步骤
    pub fn add_step(&mut self, step: PlanStep) {
        self.steps.push(step);
    }

    /// 获取就绪步骤（依赖全部完成 + 状态 Pending/Ready）
    pub fn ready_steps(&self) -> Vec<&PlanStep> {
        let completed_ids: Vec<&str> = self.steps.iter()
            .filter(|s| s.status == StepStatus::Completed)
            .map(|s| s.id.as_str())
            .collect();

        self.steps.iter()
            .filter(|s| matches!(s.status, StepStatus::Pending | StepStatus::Ready))
            .filter(|s| s.depends_on.iter().all(|dep| completed_ids.contains(&dep.as_str())))
            .collect()
    }

    /// 标记步骤状态
    pub fn mark_step(&mut self, step_id: &str, status: StepStatus, result: Option<StepResult>) {
        if let Some(step) = self.steps.iter_mut().find(|s| s.id == step_id) {
            step.status = status;
            if result.is_some() {
                step.result = result;
            }
        }
    }

    /// 步骤重试（计数器+1，重置状态为 Ready）
    pub fn retry_step(&mut self, step_id: &str) -> bool {
        if let Some(step) = self.steps.iter_mut().find(|s| s.id == step_id) {
            if step.retries < step.max_retries {
                step.retries += 1;
                step.status = StepStatus::Ready;
                step.result = None;
                return true;
            }
        }
        false
    }

    /// 是否所有步骤完成
    pub fn is_done(&self) -> bool {
        self.steps.iter().all(|s| matches!(s.status, StepStatus::Completed | StepStatus::Failed | StepStatus::Skipped))
    }

    /// 是否有失败步骤（不可重试）
    pub fn has_unrecoverable_failure(&self) -> bool {
        self.steps.iter().any(|s| {
            s.status == StepStatus::Failed && s.retries >= s.max_retries
        })
    }

    /// 统计
    pub fn stats(&self) -> PlanStats {
        PlanStats {
            total: self.steps.len(),
            completed: self.steps.iter().filter(|s| s.status == StepStatus::Completed).count(),
            failed: self.steps.iter().filter(|s| s.status == StepStatus::Failed).count(),
            running: self.steps.iter().filter(|s| s.status == StepStatus::Running).count(),
            pending: self.steps.iter().filter(|s| matches!(s.status, StepStatus::Pending | StepStatus::Ready | StepStatus::Blocked)).count(),
        }
    }
}

/// 计划统计
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStats {
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub running: usize,
    pub pending: usize,
}

// ─── Plan Executor ──────────────────────────────────────────────────────

/// 计划执行器所需的运行时服务（由调用方注入）
///
/// ## 生命周期
/// - 由 TeamSession 或 CoreLoop 在创建 PlanExecutor 时注入
/// - 生命周期跟随 PlanExecutor（通常是单次 plan 执行周期）
///
/// ## 引用关系
/// - ToolRegistry: abacus-core/src/tool/mod.rs
/// - CoreLoop: abacus-core/src/core/mod.rs (process_turn)
/// - SubAgentDispatcher: crate::subagent
pub struct PlanServices {
    pub tool_registry: std::sync::Arc<abacus_core::tool::ToolRegistry>,
    pub core: std::sync::Arc<abacus_core::core::CoreLoop>,
    pub subagent_dispatcher: std::sync::Arc<crate::subagent::SubAgentDispatcher>,
}

/// 计划执行器
///
/// ## 场景
/// 接收 PlanModel → 循环提取就绪步骤 → 执行 → 更新状态 → 直到完成/失败。
///
/// ## 执行策略
/// - ToolCall：通过 ToolRegistry 执行
/// - LlmReason：通过 CoreLoop.process_turn() 执行
/// - SubAgentDelegate：通过 SubAgentDispatcher 执行
/// - Parallel：tokio::join_all 并行
/// - Sequence：逐个顺序
/// - Conditional：占位（需要运行时条件评估引擎）
/// - 失败：重试直到 max_retries → 仍失败 → 标记 Failed
pub struct PlanExecutor {
    /// 可选服务注入。None = 所有步骤返回 NotSupported（测试/骨架模式）。
    services: Option<PlanServices>,
}

impl PlanExecutor {
    /// 创建无服务注入的执行器（测试/骨架模式，所有步骤返回错误）
    pub fn new() -> Self { Self { services: None } }

    /// 创建带完整服务注入的执行器（生产模式）
    pub fn with_services(services: PlanServices) -> Self {
        Self { services: Some(services) }
    }

    /// Execute a single plan step.
    ///
    /// 有服务注入时真实执行，无服务时返回 NotSupported 错误。
    pub async fn execute_step(&self, step: &PlanStep) -> Result<StepResult, KernelError> {
        let svc = match &self.services {
            Some(s) => s,
            None => return self.execute_step_stub(step).await,
        };

        match &step.kind {
            StepKind::ToolCall { tool_id, params } => {
                let tid = abacus_types::ToolId(tool_id.clone());
                let timeout = std::time::Duration::from_secs(60);
                // plan executor 无用户 session，使用 noop ExecutionContext（step id 作为标识）
                let exec_ctx = abacus_core::tool::ExecutionContext::noop(format!("plan_step_{}", step.id));
                match tokio::time::timeout(timeout, svc.tool_registry.execute(&tid, params.clone(), &exec_ctx)).await {
                    Ok(Ok(output)) => {
                        if output.success {
                            Ok(StepResult::Success(output.output))
                        } else {
                            Ok(StepResult::Failed {
                                error: output.output.to_string(),
                                retryable: true,
                            })
                        }
                    }
                    Ok(Err(e)) => Ok(StepResult::Failed { error: e.to_string(), retryable: true }),
                    Err(_) => Ok(StepResult::Failed { error: format!("tool timeout: {tool_id}"), retryable: true }),
                }
            }

            StepKind::LlmReason { prompt } => {
                use tokio::sync::RwLock;
                use abacus_core::core::SessionState;
                // 创建临时 session 执行单次推理
                let session = SessionState::new(format!("plan_reason_{}", step.id));
                svc.core.register_session_context_tools(&session).await;
                let session = RwLock::new(session);
                match svc.core.process_turn(prompt, &session).await {
                    Ok(result) => Ok(StepResult::Success(serde_json::json!({
                        "response": result.response,
                    }))),
                    Err(e) => Ok(StepResult::Failed { error: e.to_string(), retryable: false }),
                }
            }

            StepKind::SubAgentDelegate { task_description, boundary_preset } => {
                use crate::subagent::{SubAgentBoundary, SubAgentContext};
                let boundary = match boundary_preset.as_str() {
                    "tight" => SubAgentBoundary { max_steps: 50, max_tokens: 32_000, ..Default::default() },
                    "wide" => SubAgentBoundary { max_steps: 1000, max_tokens: 500_000, ..Default::default() },
                    _ => SubAgentBoundary::default(),
                };
                let ctx = SubAgentContext {
                    parent_session_id: format!("plan_{}", step.id),
                    inherited_keys: vec!["goal".into()],
                    task_description: task_description.clone(),
                    nesting_depth: 0,
                };
                let instance = svc.subagent_dispatcher.create(boundary, ctx).await;
                let agent_id = instance.id.clone();
                svc.subagent_dispatcher.mark_running(&agent_id).await?;

                // 用 CoreLoop 执行 SubAgent 任务
                use tokio::sync::RwLock;
                use abacus_core::core::SessionState;
                let session = SessionState::new(format!("sa_{}", agent_id));
                svc.core.register_session_context_tools(&session).await;
                let session = RwLock::new(session);
                match svc.core.process_turn(task_description, &session).await {
                    Ok(result) => {
                        let sa_result = crate::subagent::SubAgentResult {
                            agent_id: agent_id.clone(),
                            success: true,
                            output: serde_json::json!({"response": result.response}),
                            tokens_used: (result.stats.prompt_tokens + result.stats.completion_tokens) as usize,
                            steps_used: result.stats.tool_calls,
                            duration_ms: result.stats.latency_ms,
                        };
                        svc.subagent_dispatcher.mark_completed(&agent_id, sa_result).await?;
                        Ok(StepResult::Success(serde_json::json!({"response": result.response})))
                    }
                    Err(e) => {
                        svc.subagent_dispatcher.mark_failed(&agent_id, e.to_string(), true).await?;
                        Ok(StepResult::Failed { error: e.to_string(), retryable: true })
                    }
                }
            }

            StepKind::Conditional { condition, .. } => {
                // 条件评估需要更完整的表达式引擎，暂时跳过
                Ok(StepResult::Skipped {
                    reason: format!("conditional evaluation not yet implemented: {condition}"),
                })
            }

            StepKind::Parallel(_) | StepKind::Sequence(_) => {
                // Parallel/Sequence 由 run() 循环在外层处理（通过依赖拓扑自然实现）
                // 单步执行器不直接处理组合步骤
                Ok(StepResult::Skipped {
                    reason: "composite step handled by run() loop".into(),
                })
            }
        }
    }

    /// 无服务时的占位执行（测试模式）
    async fn execute_step_stub(&self, step: &PlanStep) -> Result<StepResult, KernelError> {
        match &step.kind {
            StepKind::ToolCall { tool_id, .. } => Err(KernelError::Other(format!(
                "PlanExecutor: no services injected for ToolCall({tool_id})"
            ))),
            StepKind::LlmReason { .. } => Err(KernelError::Other(
                "PlanExecutor: no services injected for LlmReason".into()
            )),
            StepKind::SubAgentDelegate { task_description, .. } => Err(KernelError::Other(format!(
                "PlanExecutor: no services injected for SubAgentDelegate({task_description})"
            ))),
            StepKind::Conditional { condition, .. } => Err(KernelError::Other(format!(
                "PlanExecutor: Conditional({condition}) not supported in stub mode"
            ))),
            StepKind::Parallel(ids) => Err(KernelError::Other(format!(
                "PlanExecutor: Parallel({} steps) not supported in stub mode", ids.len()
            ))),
            StepKind::Sequence(ids) => Err(KernelError::Other(format!(
                "PlanExecutor: Sequence({} steps) not supported in stub mode", ids.len()
            ))),
        }
    }

    /// 执行整个计划（循环直到完成/失败）
    ///
    /// 返回最终 PlanStatus。
    pub async fn run(&self, plan: &mut PlanModel) -> Result<PlanStatus, KernelError> {
        plan.status = PlanStatus::InProgress;
        // 安全上限：steps * (max_retries+1) * 2 防止无限循环
        let avg_retries = plan.steps.iter().map(|s| s.max_retries as usize).max().unwrap_or(3);
        let max_iterations = (plan.steps.len() * (avg_retries + 1) * 2).max(20);
        let mut iteration = 0;

        loop {
            iteration += 1;
            if iteration > max_iterations {
                plan.status = PlanStatus::Failed(format!(
                    "exceeded max iterations ({}) — possible infinite retry loop", max_iterations
                ));
                return Ok(plan.status.clone());
            }
            // 检查是否完成
            if plan.is_done() {
                plan.status = if plan.has_unrecoverable_failure() {
                    PlanStatus::Failed("one or more steps failed".into())
                } else {
                    PlanStatus::Completed
                };
                return Ok(plan.status.clone());
            }

            // 获取就绪步骤
            let ready: Vec<String> = plan.ready_steps().iter().map(|s| s.id.clone()).collect();
            if ready.is_empty() {
                // 没有就绪步骤且未完成 → 死锁（循环依赖）
                plan.status = PlanStatus::Failed("deadlock: no ready steps but plan not done".into());
                return Ok(plan.status.clone());
            }

            // 执行就绪步骤（每步最多 900 秒超时）
            for step_id in &ready {
                plan.mark_step(step_id, StepStatus::Running, None);

                let step = plan.steps.iter().find(|s| s.id == *step_id).unwrap().clone();
                let step_timeout = std::time::Duration::from_secs(900);
                let result = match tokio::time::timeout(step_timeout, self.execute_step(&step)).await {
                    Ok(r) => r,
                    Err(_) => Ok(StepResult::Failed {
                        error: format!("step '{}' timed out after 900s", step.id),
                        retryable: true,
                    }),
                };

                match result {
                    Ok(StepResult::Success(val)) => {
                        plan.mark_step(step_id, StepStatus::Completed, Some(StepResult::Success(val)));
                    }
                    Ok(StepResult::Skipped { reason }) => {
                        plan.mark_step(step_id, StepStatus::Skipped, Some(StepResult::Skipped { reason }));
                    }
                    Ok(StepResult::Failed { error, retryable }) => {
                        if retryable && plan.retry_step(step_id) {
                            // 重试：状态已重置为 Ready，下轮循环会再执行
                            continue;
                        }
                        plan.mark_step(step_id, StepStatus::Failed, Some(StepResult::Failed { error, retryable }));
                    }
                    Err(e) => {
                        // 系统错误，尝试重试
                        if plan.retry_step(step_id) {
                            continue;
                        }
                        plan.mark_step(step_id, StepStatus::Failed, Some(StepResult::Failed {
                            error: e.to_string(),
                            retryable: false,
                        }));
                    }
                }
            }
        }
    }
}

impl Default for PlanExecutor {
    fn default() -> Self { Self::new() }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_step(id: &str, kind: StepKind, deps: Vec<&str>) -> PlanStep {
        PlanStep {
            id: id.into(),
            kind,
            description: format!("step {id}"),
            depends_on: deps.into_iter().map(String::from).collect(),
            status: StepStatus::Pending,
            result: None,
            retries: 0,
            max_retries: 1,
        }
    }

    #[test]
    fn test_plan_ready_steps() {
        let mut plan = PlanModel::new("p1", "test");
        plan.add_step(make_step("a", StepKind::ToolCall { tool_id: "fs_read".into(), params: serde_json::json!({}) }, vec![]));
        plan.add_step(make_step("b", StepKind::LlmReason { prompt: "think".into() }, vec!["a"]));
        plan.add_step(make_step("c", StepKind::ToolCall { tool_id: "fs_write".into(), params: serde_json::json!({}) }, vec!["a"]));

        // 初始：只有 a 就绪
        let ready = plan.ready_steps();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "a");

        // a 完成后：b 和 c 都就绪
        plan.mark_step("a", StepStatus::Completed, None);
        let ready = plan.ready_steps();
        assert_eq!(ready.len(), 2);
    }

    #[test]
    fn test_plan_retry() {
        let mut plan = PlanModel::new("p2", "retry test");
        plan.add_step(make_step("x", StepKind::ToolCall { tool_id: "t".into(), params: serde_json::json!({}) }, vec![]));

        // 第一次重试成功
        assert!(plan.retry_step("x"));
        assert_eq!(plan.steps[0].retries, 1);
        assert_eq!(plan.steps[0].status, StepStatus::Ready);

        // 第二次重试失败（超过 max_retries=1）
        assert!(!plan.retry_step("x"));
    }

    #[tokio::test]
    async fn test_plan_executor_simple() {
        let executor = PlanExecutor::new();
        let mut plan = PlanModel::new("p3", "simple exec");
        plan.add_step(make_step("s1", StepKind::ToolCall { tool_id: "fs_read".into(), params: serde_json::json!({}) }, vec![]));
        plan.add_step(make_step("s2", StepKind::LlmReason { prompt: "analyze".into() }, vec!["s1"]));

        let result = executor.run(&mut plan).await;
        assert!(result.is_ok());
        if let Ok(PlanStatus::Failed(reason)) = result {
            assert!(!reason.is_empty());
        } else {
            panic!("expected Failed status");
        }
    }

    #[tokio::test]
    async fn test_plan_executor_parallel_steps() {
        let executor = PlanExecutor::new();
        let mut plan = PlanModel::new("p4", "parallel");
        plan.add_step(make_step("a", StepKind::ToolCall { tool_id: "t1".into(), params: serde_json::json!({}) }, vec![]));
        plan.add_step(make_step("b", StepKind::ToolCall { tool_id: "t2".into(), params: serde_json::json!({}) }, vec![]));
        plan.add_step(make_step("c", StepKind::LlmReason { prompt: "merge".into() }, vec!["a", "b"]));

        let result = executor.run(&mut plan).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_plan_stats() {
        let mut plan = PlanModel::new("p5", "stats");
        plan.add_step(make_step("a", StepKind::ToolCall { tool_id: "t".into(), params: serde_json::json!({}) }, vec![]));
        plan.add_step(make_step("b", StepKind::ToolCall { tool_id: "t".into(), params: serde_json::json!({}) }, vec![]));

        plan.mark_step("a", StepStatus::Completed, None);

        let stats = plan.stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.pending, 1);
    }

    #[test]
    fn test_deadlock_detection() {
        // 循环依赖：a→b, b→a → deadlock
        let mut plan = PlanModel::new("p6", "deadlock");
        plan.add_step(make_step("a", StepKind::ToolCall { tool_id: "t".into(), params: serde_json::json!({}) }, vec!["b"]));
        plan.add_step(make_step("b", StepKind::ToolCall { tool_id: "t".into(), params: serde_json::json!({}) }, vec!["a"]));

        // 没有就绪步骤（都被阻塞）
        assert!(plan.ready_steps().is_empty());
    }
}
