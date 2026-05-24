//! subagent — SubAgent 执行引擎与边界强制
//!
//! ## 场景
//! 接收 TaskSpec → 创建隔离执行环境 → 强制边界（token/step/duration）→ 返回结果。
//! SubAgent 在同一 tokio runtime 内运行，不启动新进程。
//!
//! ## 依赖
//! - `abacus_types`: KernelError, ToolId
//! - `tokio::time`: 超时控制
//! - `crate::team`: TaskSpec
//!
//! ## 引用关系
//! - 被 `team::TeamSession` 在执行阶段调用
//! - 被 `plan::PlanExecutor` 在 SubAgentDelegate 步骤调用
//!
//! ## 生命周期
//! SubAgentInstance: Pending → Running → Completed/Failed/Aborted

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use abacus_types::{KernelError, ToolId};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

// ─── Boundary Definition ────────────────────────────────────────────────

/// 上下文访问范围
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContextScope {
    /// 只读访问父级上下文
    ReadOnly,
    /// 读写指定 key
    ReadWrite { allowed_keys: Vec<String> },
    /// 完全隔离（不继承任何上下文）
    Isolated,
    /// 继承指定 key（只读副本）
    Inherited { keys: Vec<String> },
}

/// SubAgent 执行边界（硬限制）
///
/// 任一条件超限 → 立即中断执行，返回 Aborted 状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentBoundary {
    /// 最大工具调用步数
    pub max_steps: u32,
    /// 最大 token 消耗（prompt + completion）
    pub max_tokens: usize,
    /// 最大执行时长
    #[serde(with = "duration_serde")]
    pub max_duration: Duration,
    /// 允许使用的工具（空 = 允许全部）
    pub allowed_tools: Vec<ToolId>,
    /// 禁止使用的工具
    pub forbidden_tools: Vec<ToolId>,
    /// 上下文访问范围
    pub context_scope: ContextScope,
    /// 是否允许嵌套 SubAgent
    pub allow_nesting: bool,
    /// 最大嵌套深度
    pub max_nesting_depth: u32,
    /// 渐进输出门控作用域（默认: TeamInExecution = 豁免）
    /// SubAgent 执行时门控不生效，仅 Leader 面向用户时生效
    #[serde(default)]
    pub progressive_gate_scope: abacus_types::progressive::GateScope,
}

impl Default for SubAgentBoundary {
    fn default() -> Self {
        Self {
            max_steps: 500,
            max_tokens: 200_000,
            max_duration: Duration::from_secs(900),
            allowed_tools: Vec::new(),
            forbidden_tools: Vec::new(),
            context_scope: ContextScope::Inherited { keys: vec!["goal".into()] },
            allow_nesting: false,
            max_nesting_depth: 0,
            progressive_gate_scope: abacus_types::progressive::GateScope::TeamInExecution,
        }
    }
}

impl SubAgentBoundary {
    /// 检查工具是否在允许范围内
    pub fn is_tool_allowed(&self, tool_id: &ToolId) -> bool {
        // forbidden 优先
        if self.forbidden_tools.contains(tool_id) {
            return false;
        }
        // allowed 为空表示允许全部
        if self.allowed_tools.is_empty() {
            return true;
        }
        self.allowed_tools.contains(tool_id)
    }
}

// ─── Context ────────────────────────────────────────────────────────────

/// SubAgent 执行上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentContext {
    pub parent_session_id: String,
    pub inherited_keys: Vec<String>,
    pub task_description: String,
    pub nesting_depth: u32,
}

// ─── Result & Status ────────────────────────────────────────────────────

/// SubAgent 执行结果
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubAgentResult {
    pub agent_id: String,
    pub success: bool,
    pub output: serde_json::Value,
    pub tokens_used: usize,
    pub steps_used: u32,
    pub duration_ms: u64,
}

/// SubAgent 状态机
///
/// ## 转换规则
/// ```text
/// Pending → Running → Completed
///    │        │ → Failed
///    │        │ → Aborted
///    │        └ → Paused → Running (resume)
///    └─────────────→ Cancelled
///
/// 终态: Completed | Failed | Aborted | Cancelled
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SubAgentStatus {
    /// 已创建，等待启动
    Pending,
    /// 正在执行
    Running { started_at_ms: u64, current_step: u32 },
    /// 暂停（等待外部输入/依赖解除）
    Paused { reason: String, at_step: u32 },
    /// 成功完成
    Completed(SubAgentResult),
    /// 执行失败（可重试）
    Failed { error: String, at_step: u32, retryable: bool },
    /// 边界超限强制中断
    Aborted { reason: AbortReason },
    /// 被外部取消（Leader 主动终止）
    Cancelled { by: String },
}

impl SubAgentStatus {
    /// 是否为终态（不可再转换）
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed(_) | Self::Failed { .. } | Self::Aborted { .. } | Self::Cancelled { .. })
    }

    /// 是否可重试
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Failed { retryable: true, .. })
    }

    /// 状态转换验证
    pub fn can_transition_to(&self, next: &SubAgentStatus) -> bool {
        match (self, next) {
            // Pending 可以 → Running | Cancelled
            (Self::Pending, Self::Running { .. }) => true,
            (Self::Pending, Self::Cancelled { .. }) => true,

            // Running 可以 → Completed | Failed | Aborted | Paused
            (Self::Running { .. }, Self::Completed(_)) => true,
            (Self::Running { .. }, Self::Failed { .. }) => true,
            (Self::Running { .. }, Self::Aborted { .. }) => true,
            (Self::Running { .. }, Self::Paused { .. }) => true,
            (Self::Running { .. }, Self::Cancelled { .. }) => true,

            // Paused 可以 → Running (恢复) | Cancelled | Failed
            (Self::Paused { .. }, Self::Running { .. }) => true,
            (Self::Paused { .. }, Self::Cancelled { .. }) => true,
            (Self::Paused { .. }, Self::Failed { .. }) => true,

            // 终态不可转换
            _ => false,
        }
    }

    /// 获取当前步数（如果有）
    pub fn current_step(&self) -> Option<u32> {
        match self {
            Self::Running { current_step, .. } => Some(*current_step),
            Self::Paused { at_step, .. } => Some(*at_step),
            Self::Failed { at_step, .. } => Some(*at_step),
            _ => None,
        }
    }

    /// 获取状态名称（用于日志/事件）
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running { .. } => "running",
            Self::Paused { .. } => "paused",
            Self::Completed(_) => "completed",
            Self::Failed { .. } => "failed",
            Self::Aborted { .. } => "aborted",
            Self::Cancelled { .. } => "cancelled",
        }
    }
}

/// 中断原因
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AbortReason {
    TokenLimitExceeded { used: usize, limit: usize },
    StepLimitExceeded { used: u32, limit: u32 },
    DurationExceeded { elapsed_ms: u64, limit_ms: u64 },
    ToolDenied { tool_id: String },
    NestingDepthExceeded { depth: u32, max: u32 },
}

impl std::fmt::Display for AbortReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TokenLimitExceeded { used, limit } => write!(f, "token limit: {used}/{limit}"),
            Self::StepLimitExceeded { used, limit } => write!(f, "step limit: {used}/{limit}"),
            Self::DurationExceeded { elapsed_ms, limit_ms } => write!(f, "duration: {elapsed_ms}ms/{limit_ms}ms"),
            Self::ToolDenied { tool_id } => write!(f, "tool denied: {tool_id}"),
            Self::NestingDepthExceeded { depth, max } => write!(f, "nesting: {depth}/{max}"),
        }
    }
}

// ─── Runtime Tracker ────────────────────────────────────────────────────

/// 运行时边界跟踪器
///
/// 每个 SubAgent 实例持有一个 tracker，每次工具调用后检查边界。
#[derive(Debug)]
pub struct BoundaryTracker {
    boundary: SubAgentBoundary,
    steps_used: AtomicU32,
    tokens_used: std::sync::atomic::AtomicUsize,
    started_at: Instant,
}

impl BoundaryTracker {
    pub fn new(boundary: SubAgentBoundary) -> Self {
        Self {
            boundary,
            steps_used: AtomicU32::new(0),
            tokens_used: std::sync::atomic::AtomicUsize::new(0),
            started_at: Instant::now(),
        }
    }

    /// 记录一个步骤（工具调用后）
    pub fn record_step(&self, tokens: usize) {
        self.steps_used.fetch_add(1, Ordering::Relaxed);
        self.tokens_used.fetch_add(tokens, Ordering::Relaxed);
    }

    /// 检查是否超限，返回 None = 正常，Some = 中断原因
    pub fn check_limits(&self) -> Option<AbortReason> {
        let steps = self.steps_used.load(Ordering::Relaxed);
        if steps >= self.boundary.max_steps {
            return Some(AbortReason::StepLimitExceeded {
                used: steps, limit: self.boundary.max_steps,
            });
        }

        let tokens = self.tokens_used.load(Ordering::Relaxed);
        if tokens >= self.boundary.max_tokens {
            return Some(AbortReason::TokenLimitExceeded {
                used: tokens, limit: self.boundary.max_tokens,
            });
        }

        let elapsed = self.started_at.elapsed();
        if elapsed >= self.boundary.max_duration {
            return Some(AbortReason::DurationExceeded {
                elapsed_ms: elapsed.as_millis() as u64,
                limit_ms: self.boundary.max_duration.as_millis() as u64,
            });
        }

        None
    }

    /// 检查工具是否允许
    pub fn check_tool(&self, tool_id: &ToolId) -> Option<AbortReason> {
        if !self.boundary.is_tool_allowed(tool_id) {
            return Some(AbortReason::ToolDenied { tool_id: tool_id.0.clone() });
        }
        None
    }

    /// 获取当前使用统计
    pub fn stats(&self) -> (u32, usize, u64) {
        (
            self.steps_used.load(Ordering::Relaxed),
            self.tokens_used.load(Ordering::Relaxed),
            self.started_at.elapsed().as_millis() as u64,
        )
    }
}

// ─── SubAgent Instance ──────────────────────────────────────────────────

/// SubAgent 实例（运行时）
#[derive(Debug)]
pub struct SubAgentInstance {
    pub id: String,
    pub boundary: SubAgentBoundary,
    pub context: SubAgentContext,
    pub status: RwLock<SubAgentStatus>,
    pub tracker: Arc<BoundaryTracker>,
    /// P2: watchdog 取消令牌。mark_completed / mark_cancelled / mark_failed
    /// 会 cancel 它，让 mark_running 中 spawn 的 watchdog 立即退出而不是空跑
    /// 完整的 max_duration sleep（避免 spawn 任务堆积）。
    pub watchdog_cancel: tokio_util::sync::CancellationToken,
}

// ─── Dispatcher ─────────────────────────────────────────────────────────

/// SubAgent 调度器
///
/// ## 场景
/// 管理 SubAgent 的创建、启动、监控、结果收集。
///
/// ## 执行方式
/// SubAgent 在同一 tokio runtime 内运行（不启动新进程），
/// 通过 BoundaryTracker 在每个 tool call 后检查限制。
pub struct SubAgentDispatcher {
    agents: RwLock<HashMap<String, Arc<SubAgentInstance>>>,
    next_id: AtomicU32,
}

impl SubAgentDispatcher {
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
            next_id: AtomicU32::new(1),
        }
    }

    /// 创建并注册 SubAgent（状态 Pending）
    pub async fn create(
        &self,
        boundary: SubAgentBoundary,
        context: SubAgentContext,
    ) -> Arc<SubAgentInstance> {
        let id = format!("sa_{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let tracker = Arc::new(BoundaryTracker::new(boundary.clone()));

        let instance = Arc::new(SubAgentInstance {
            id: id.clone(),
            boundary,
            context,
            status: RwLock::new(SubAgentStatus::Pending),
            tracker,
            watchdog_cancel: tokio_util::sync::CancellationToken::new(),
        });

        self.agents.write().await.insert(id, instance.clone());
        instance
    }

    /// 标记为运行中，并启动超时守卫 (max_duration 到达时自动 abort)。
    pub async fn mark_running(&self, id: &str) -> Result<(), KernelError> {
        let agents = self.agents.read().await;
        let agent = agents.get(id)
            .ok_or_else(|| KernelError::Other(format!("agent not found: {id}")))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        *agent.status.write().await = SubAgentStatus::Running { started_at_ms: now, current_step: 0 };

        // P2: Spawn timeout watchdog with cancellation. mark_completed/cancelled/failed
        // 会 cancel watchdog token，避免 spawn 任务空跑 max_duration（资源浪费）。
        let timeout_dur = agent.boundary.max_duration;
        let agent_ref = agent.clone();
        let cancel = agent.watchdog_cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(timeout_dur) => {
                    // 超时到达：仅当仍在 Running 时升级为 Aborted
                    let status = agent_ref.status.read().await;
                    if matches!(*status, SubAgentStatus::Running { .. }) {
                        drop(status);
                        *agent_ref.status.write().await = SubAgentStatus::Aborted {
                            reason: AbortReason::DurationExceeded {
                                elapsed_ms: timeout_dur.as_millis() as u64,
                                limit_ms: timeout_dur.as_millis() as u64,
                            },
                        };
                    }
                }
                _ = cancel.cancelled() => {
                    // 提前完成或被取消：watchdog 立即退出
                }
            }
        });
        Ok(())
    }

    /// 标记为完成
    pub async fn mark_completed(&self, id: &str, result: SubAgentResult) -> Result<(), KernelError> {
        let agents = self.agents.read().await;
        let agent = agents.get(id)
            .ok_or_else(|| KernelError::Other(format!("agent not found: {id}")))?;
        *agent.status.write().await = SubAgentStatus::Completed(result);
        // P2: 提前完成时唤醒 watchdog 立即退出（避免空跑 max_duration）
        agent.watchdog_cancel.cancel();
        Ok(())
    }

    /// 标记为失败
    pub async fn mark_failed(&self, id: &str, error: String, retryable: bool) -> Result<(), KernelError> {
        let agents = self.agents.read().await;
        let agent = agents.get(id)
            .ok_or_else(|| KernelError::Other(format!("agent not found: {id}")))?;
        let at_step = {
            let status = agent.status.read().await;
            status.current_step().unwrap_or(0)
        };
        *agent.status.write().await = SubAgentStatus::Failed { error, at_step, retryable };
        agent.watchdog_cancel.cancel();
        Ok(())
    }

    /// 标记为暂停
    pub async fn mark_paused(&self, id: &str, reason: String) -> Result<(), KernelError> {
        let agents = self.agents.read().await;
        let agent = agents.get(id)
            .ok_or_else(|| KernelError::Other(format!("agent not found: {id}")))?;
        let at_step = {
            let status = agent.status.read().await;
            status.current_step().unwrap_or(0)
        };
        *agent.status.write().await = SubAgentStatus::Paused { reason, at_step };
        Ok(())
    }

    /// 恢复执行（从 Paused → Running）
    pub async fn mark_resumed(&self, id: &str) -> Result<(), KernelError> {
        let agents = self.agents.read().await;
        let agent = agents.get(id)
            .ok_or_else(|| KernelError::Other(format!("agent not found: {id}")))?;
        let (at_step, is_paused) = {
            let status = agent.status.read().await;
            (status.current_step().unwrap_or(0), matches!(*status, SubAgentStatus::Paused { .. }))
        };
        if !is_paused {
            return Err(KernelError::Other(format!("agent {id} is not paused, cannot resume")));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        *agent.status.write().await = SubAgentStatus::Running { started_at_ms: now, current_step: at_step };
        Ok(())
    }

    /// 取消执行
    pub async fn mark_cancelled(&self, id: &str, by: String) -> Result<(), KernelError> {
        let agents = self.agents.read().await;
        let agent = agents.get(id)
            .ok_or_else(|| KernelError::Other(format!("agent not found: {id}")))?;
        let status = agent.status.read().await;
        if status.is_terminal() {
            return Err(KernelError::Other(format!("agent {id} already in terminal state")));
        }
        drop(status);
        *agent.status.write().await = SubAgentStatus::Cancelled { by };
        // P2: 唤醒 watchdog 立即退出
        agent.watchdog_cancel.cancel();
        Ok(())
    }

    /// 更新当前步数（Running 状态下每步工具调用后）
    pub async fn advance_step(&self, id: &str) -> Result<u32, KernelError> {
        let agents = self.agents.read().await;
        let agent = agents.get(id)
            .ok_or_else(|| KernelError::Other(format!("agent not found: {id}")))?;
        let mut status = agent.status.write().await;
        match &mut *status {
            SubAgentStatus::Running { current_step, .. } => {
                *current_step += 1;
                Ok(*current_step)
            }
            _ => Err(KernelError::Other(format!("agent {id} not running, cannot advance"))),
        }
    }

    /// 标记为中断
    pub async fn mark_aborted(&self, id: &str, reason: AbortReason) -> Result<(), KernelError> {
        let agents = self.agents.read().await;
        let agent = agents.get(id)
            .ok_or_else(|| KernelError::Other(format!("agent not found: {id}")))?;
        *agent.status.write().await = SubAgentStatus::Aborted { reason };
        Ok(())
    }

    /// 获取 SubAgent 实例
    pub async fn get(&self, id: &str) -> Option<Arc<SubAgentInstance>> {
        self.agents.read().await.get(id).cloned()
    }

    /// 获取状态
    pub async fn status(&self, id: &str) -> Option<SubAgentStatus> {
        let agent = {
            let agents = self.agents.read().await;
            agents.get(id).cloned()
        }?;
        let status = agent.status.read().await.clone();
        Some(status)
    }

    /// 获取所有活跃（Running）的 agent
    pub async fn active_agents(&self) -> Vec<String> {
        let agent_list: Vec<(String, Arc<SubAgentInstance>)> = {
            let agents = self.agents.read().await;
            agents.iter().map(|(id, a)| (id.clone(), a.clone())).collect()
        };
        let mut active = Vec::new();
        for (id, agent) in &agent_list {
            let status = agent.status.read().await;
            if matches!(*status, SubAgentStatus::Running { .. }) {
                active.push(id.clone());
            }
        }
        active
    }

    /// 清理已完成的 agent——用 try_read 避免 async 在 retain 闭包中
    pub async fn cleanup_completed(&self) -> usize {
        let mut agents = self.agents.write().await;
        let before = agents.len();
        agents.retain(|_, instance| {
            // try_read: 非阻塞读取，失败则保留（保守策略）
            match instance.status.try_read() {
                Ok(status) => !status.is_terminal(),
                Err(_) => true, // 无法读取 → 保留
            }
        });
        before - agents.len()
    }

    /// 移除指定 agent
    pub async fn remove(&self, id: &str) -> bool {
        self.agents.write().await.remove(id).is_some()
    }
}

impl Default for SubAgentDispatcher {
    fn default() -> Self { Self::new() }
}

// ─── Duration Serde Helper ──────────────────────────────────────────────

mod duration_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where S: Serializer {
        duration.as_secs().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where D: Deserializer<'de> {
        let secs = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(secs))
    }
}

// ─── SubAgent Context Manager ─────────────────────────────────────────

/// SubAgent 上下文管理器
///
/// ## 场景
/// 管理 SubAgent 的 context window 生命周期：
/// 1. 注入：从父级上下文按 ContextScope 提取相关信息注入
/// 2. 管理：运行时监控 token 消耗，接近边界时触发压缩
/// 3. 压缩：保留任务目标 + 最近 3 轮 + 工具结果摘要
/// 4. 重注入：压缩后的摘要重新注入为 system prompt 的一部分
///
/// ## 生命周期
/// 每个 SubAgentInstance 持有一个 ContextManager
/// 随 SubAgent 完成/中断而释放
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentContextManager {
    /// 初始注入的父级上下文摘要
    pub injected_context: String,
    /// 任务目标（压缩时永不丢弃）
    pub task_goal: String,
    /// 当前 token 使用量估算
    pub current_tokens: usize,
    /// token 阈值（超过此值触发压缩）
    pub compression_threshold: usize,
    /// 压缩历史记录
    pub compression_history: Vec<CompressionRecord>,
    /// 是否处于压缩后状态
    pub is_compressed: bool,
}

/// 压缩记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionRecord {
    pub at_step: u32,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub turns_compressed: usize,
    pub summary: String,
}

impl SubAgentContextManager {
    /// 创建上下文管理器
    ///
    /// `parent_context`: 从父级按 ContextScope 提取的上下文文本
    /// `task_goal`: 任务描述（压缩时保留）
    /// `max_tokens`: SubAgentBoundary.max_tokens
    pub fn new(parent_context: String, task_goal: String, max_tokens: usize) -> Self {
        let initial_tokens = estimate_tokens(&parent_context) + estimate_tokens(&task_goal);
        Self {
            injected_context: parent_context,
            task_goal,
            current_tokens: initial_tokens,
            compression_threshold: (max_tokens as f64 * 0.75) as usize, // 75% 触发
            compression_history: Vec::new(),
            is_compressed: false,
        }
    }

    /// 生成注入到 SubAgent system prompt 的上下文块
    ///
    /// ## 注入策略
    /// - 任务目标：始终在最前（不可压缩）
    /// - 父级上下文：摘要形式注入
    /// - 压缩摘要：如果已压缩，附加历史摘要
    pub fn build_system_context(&self) -> String {
        let mut ctx = String::new();

        // 不可压缩部分
        ctx.push_str(&format!("[TASK GOAL] {}\n\n", self.task_goal));

        // 父级上下文
        if !self.injected_context.is_empty() {
            ctx.push_str(&format!("[INHERITED CONTEXT]\n{}\n\n", self.injected_context));
        }

        // 压缩摘要（如果有）
        if let Some(last) = self.compression_history.last() {
            ctx.push_str(&format!(
                "[COMPRESSED HISTORY] (前 {} 轮已压缩)\n{}\n\n",
                last.turns_compressed, last.summary
            ));
        }

        ctx
    }

    /// 记录一轮执行的 token 消耗
    pub fn record_turn_tokens(&mut self, tokens: usize) {
        self.current_tokens += tokens;
    }

    /// 检查是否需要压缩
    pub fn needs_compression(&self) -> bool {
        self.current_tokens >= self.compression_threshold
    }

    /// 执行压缩
    ///
    /// ## 压缩策略
    /// - 保留：task_goal（不可压缩）+ 最近 3 轮对话 + 工具结果摘要
    /// - 丢弃：早期对话细节（替换为 summary）
    /// - 重注入：压缩后的 summary 进入 compression_history
    ///
    /// `conversation_summary`: 由调用方（CoreLoop）生成的前 N 轮摘要
    /// `step_count`: 当前执行步数
    /// `turns_compressed`: 被压缩的轮数
    pub fn compress(
        &mut self,
        conversation_summary: String,
        step_count: u32,
        turns_compressed: usize,
    ) {
        let tokens_before = self.current_tokens;
        let summary_tokens = estimate_tokens(&conversation_summary);

        // 更新 token 计数（压缩后 = 固定部分 + 摘要 + 最近 3 轮估算）
        let fixed_tokens = estimate_tokens(&self.task_goal) + estimate_tokens(&self.injected_context);
        let recent_estimate = (self.compression_threshold as f64 * 0.3) as usize; // 最近 3 轮约占 30%
        self.current_tokens = fixed_tokens + summary_tokens + recent_estimate;

        self.compression_history.push(CompressionRecord {
            at_step: step_count,
            tokens_before,
            tokens_after: self.current_tokens,
            turns_compressed,
            summary: conversation_summary,
        });

        self.is_compressed = true;
    }

    /// 获取压缩统计
    pub fn compression_stats(&self) -> (usize, usize) {
        (
            self.compression_history.len(),
            self.compression_history.iter().map(|r| r.turns_compressed).sum(),
        )
    }
}

/// Token 估算辅助（CJK-aware，与 abacus-core::core::context::estimate_tokens 一致）
fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() { return 1; }
    let mut cjk_chars = 0usize;
    let mut ascii_bytes = 0usize;
    for ch in text.chars() {
        if matches!(ch,
            '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' |
            '\u{F900}'..='\u{FAFF}' | '\u{3000}'..='\u{303F}' |
            '\u{FF00}'..='\u{FFEF}' | '\u{AC00}'..='\u{D7AF}' |
            '\u{3040}'..='\u{309F}' | '\u{30A0}'..='\u{30FF}'
        ) {
            cjk_chars += 1;
        } else {
            ascii_bytes += ch.len_utf8();
        }
    }
    let cjk_tokens = (cjk_chars as f64 * 1.2) as usize;
    let ascii_tokens = (ascii_bytes as f64 * 0.25) as usize;
    let whitespace_bonus = text.split_whitespace().count() / 4;
    (cjk_tokens + ascii_tokens + whitespace_bonus).max(1)
}

// ─── Context Injection Builder ───────────────────────────────────────

/// 上下文注入构建器
///
/// 根据 ContextScope 从父级 SharedContext 提取相关信息。
pub struct ContextInjectionBuilder;

impl ContextInjectionBuilder {
    /// 根据 scope 从父级上下文生成注入文本
    ///
    /// - ReadOnly: 全量只读副本
    /// - ReadWrite: 指定 key 的内容
    /// - Isolated: 空（不继承任何上下文）
    /// - Inherited: 指定 key 的只读副本
    pub fn build_from_scope(
        scope: &ContextScope,
        shared_goal: &str,
        shared_facts: &[String],
        shared_decisions: &[String],
    ) -> String {
        match scope {
            ContextScope::Isolated => String::new(),

            ContextScope::ReadOnly => {
                let mut ctx = format!("Goal: {}\n", shared_goal);
                if !shared_facts.is_empty() {
                    ctx.push_str("Known facts:\n");
                    for f in shared_facts {
                        ctx.push_str(&format!("- {}\n", f));
                    }
                }
                if !shared_decisions.is_empty() {
                    ctx.push_str("Decisions made:\n");
                    for d in shared_decisions {
                        ctx.push_str(&format!("- {}\n", d));
                    }
                }
                ctx
            }

            ContextScope::ReadWrite { allowed_keys } => {
                let mut ctx = String::new();
                if allowed_keys.contains(&"goal".to_string()) {
                    ctx.push_str(&format!("Goal: {}\n", shared_goal));
                }
                if allowed_keys.contains(&"facts".to_string()) {
                    for f in shared_facts {
                        ctx.push_str(&format!("- {}\n", f));
                    }
                }
                if allowed_keys.contains(&"decisions".to_string()) {
                    for d in shared_decisions {
                        ctx.push_str(&format!("- {}\n", d));
                    }
                }
                ctx
            }

            ContextScope::Inherited { keys } => {
                let mut ctx = String::new();
                if keys.contains(&"goal".to_string()) {
                    ctx.push_str(&format!("Goal: {}\n", shared_goal));
                }
                if keys.contains(&"facts".to_string()) {
                    for f in shared_facts {
                        ctx.push_str(&format!("- {}\n", f));
                    }
                }
                ctx
            }
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_boundary_tool_check() {
        let boundary = SubAgentBoundary {
            allowed_tools: vec![ToolId("filengine_fs_read".into()), ToolId("filengine_fs_write".into())],
            forbidden_tools: vec![ToolId("filengine_bash_exec".into())],
            ..Default::default()
        };

        assert!(boundary.is_tool_allowed(&ToolId("filengine_fs_read".into())));
        assert!(!boundary.is_tool_allowed(&ToolId("filengine_web_search".into()))); // not in allowed
        assert!(!boundary.is_tool_allowed(&ToolId("filengine_bash_exec".into()))); // explicitly forbidden
    }

    #[test]
    fn test_boundary_empty_allowed_means_all() {
        let boundary = SubAgentBoundary {
            allowed_tools: vec![], // empty = allow all
            forbidden_tools: vec![ToolId("filengine_bash_exec".into())],
            ..Default::default()
        };

        assert!(boundary.is_tool_allowed(&ToolId("filengine_fs_read".into())));
        assert!(boundary.is_tool_allowed(&ToolId("filengine_web_search".into())));
        assert!(!boundary.is_tool_allowed(&ToolId("filengine_bash_exec".into()))); // still forbidden
    }

    #[test]
    fn test_boundary_tracker_step_limit() {
        let boundary = SubAgentBoundary { max_steps: 3, ..Default::default() };
        let tracker = BoundaryTracker::new(boundary);

        assert!(tracker.check_limits().is_none());
        tracker.record_step(100);
        tracker.record_step(100);
        assert!(tracker.check_limits().is_none());
        tracker.record_step(100);
        assert!(matches!(tracker.check_limits(), Some(AbortReason::StepLimitExceeded { .. })));
    }

    #[test]
    fn test_boundary_tracker_token_limit() {
        let boundary = SubAgentBoundary { max_tokens: 500, ..Default::default() };
        let tracker = BoundaryTracker::new(boundary);

        tracker.record_step(200);
        assert!(tracker.check_limits().is_none());
        tracker.record_step(400); // total 600 > 500
        assert!(matches!(tracker.check_limits(), Some(AbortReason::TokenLimitExceeded { .. })));
    }

    #[test]
    fn test_boundary_tracker_tool_denied() {
        let boundary = SubAgentBoundary {
            forbidden_tools: vec![ToolId("filengine_bash_exec".into())],
            ..Default::default()
        };
        let tracker = BoundaryTracker::new(boundary);

        assert!(tracker.check_tool(&ToolId("filengine_fs_read".into())).is_none());
        assert!(matches!(
            tracker.check_tool(&ToolId("filengine_bash_exec".into())),
            Some(AbortReason::ToolDenied { .. })
        ));
    }

    #[tokio::test]
    async fn test_dispatcher_lifecycle() {
        let dispatcher = SubAgentDispatcher::new();

        let ctx = SubAgentContext {
            parent_session_id: "parent_1".into(),
            inherited_keys: vec!["goal".into()],
            task_description: "test task".into(),
            nesting_depth: 0,
        };
        let boundary = SubAgentBoundary::default();

        let instance = dispatcher.create(boundary, ctx).await;
        let id = instance.id.clone();

        // 初始状态 Pending
        let status = dispatcher.status(&id).await.unwrap();
        assert_eq!(status, SubAgentStatus::Pending);

        // Running
        dispatcher.mark_running(&id).await.unwrap();
        let status = dispatcher.status(&id).await.unwrap();
        assert!(matches!(status, SubAgentStatus::Running { .. }));

        // Completed
        let result = SubAgentResult {
            agent_id: id.clone(),
            success: true,
            output: serde_json::json!({"answer": 42}),
            tokens_used: 256,
            steps_used: 3,
            duration_ms: 1500,
        };
        dispatcher.mark_completed(&id, result).await.unwrap();

        let status = dispatcher.status(&id).await.unwrap();
        assert!(matches!(status, SubAgentStatus::Completed(_)));
    }

    #[tokio::test]
    async fn test_dispatcher_abort() {
        let dispatcher = SubAgentDispatcher::new();
        let ctx = SubAgentContext {
            parent_session_id: "p".into(),
            inherited_keys: vec![],
            task_description: "t".into(),
            nesting_depth: 0,
        };

        let instance = dispatcher.create(SubAgentBoundary::default(), ctx).await;
        let id = instance.id.clone();

        dispatcher.mark_running(&id).await.unwrap();
        dispatcher.mark_aborted(&id, AbortReason::TokenLimitExceeded { used: 9000, limit: 8192 }).await.unwrap();

        let status = dispatcher.status(&id).await.unwrap();
        assert!(matches!(status, SubAgentStatus::Aborted { .. }));
    }
}
