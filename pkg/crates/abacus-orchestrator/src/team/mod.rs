//! team — 团队协作系统 (Mode 2)
//!
//! ## 场景
//! 多 Agent 协作执行复杂任务：Leader 分解目标 → 分配给角色 → SubAgent 执行 → 结果合并。
//!
//! ## 依赖
//! - `abacus_types`: KernelError, ToolId
//! - `crate::subagent`: SubAgentBoundary, SubAgentResult, SubAgentDispatcher
//! - `tokio::sync`: RwLock, broadcast
//!
//! ## 引用关系
//! - 被 `lib.rs` 作为 pub mod 导出
//! - 被 CLI `team` 命令调用创建/管理 team session
//! - 内部调用 SubAgentDispatcher 执行任务
//!
//! ## 生命周期
//! TeamSession: create → plan → execute → review → complete/fail

use std::collections::HashMap;
use std::sync::Arc;

use abacus_types::KernelError;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};

use crate::subagent::{SubAgentBoundary, SubAgentResult};

// ─── Agent Roles ────────────────────────────────────────────────────────

/// 团队角色
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentRole {
    /// 决策者：分解目标、解决冲突、最终审批
    Leader,
    /// 项目管理：跟踪进度、依赖管理、质量把关
    PM,
    /// 顾问：提供专业意见，不直接执行
    Advisor,
    /// 执行者：完成具体任务
    Member,
}

impl std::fmt::Display for AgentRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Leader => write!(f, "Leader"),
            Self::PM => write!(f, "PM"),
            Self::Advisor => write!(f, "Advisor"),
            Self::Member => write!(f, "Member"),
        }
    }
}

// ─── Task System ────────────────────────────────────────────────────────

/// 任务规格
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub id: String,
    pub description: String,
    pub required_capabilities: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub priority: u32,              // 0 = 最高
    pub depends_on: Vec<String>,    // 依赖的 task id 列表
    pub required_role: Option<AgentRole>, // 指定执行角色，None = 任意 Member
}

/// 任务状态
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    Blocked { waiting_on: Vec<String> },
    Assigned { role: AgentRole, agent_id: String },
    Running { agent_id: String },
    Completed { result: serde_json::Value },
    Failed { error: String, retries: u32 },
}

/// 任务实例（TaskSpec + 运行时状态）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInstance {
    pub spec: TaskSpec,
    pub status: TaskStatus,
    pub assigned_to: Option<AgentRole>,
    pub result: Option<SubAgentResult>,
}

// ─── Team Status Machine ────────────────────────────────────────────────

/// 团队 session 状态机
///
/// 状态转换：
/// Created → Planning → Executing → Reviewing → Completed
///                  ↘ Failed ↙         ↗ Failed
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TeamStatus {
    /// 刚创建，等待 Leader 分解任务
    Created,
    /// Leader 正在分解目标为 TaskSpec 列表
    Planning,
    /// SubAgent 正在执行任务
    Executing { active_tasks: usize, completed_tasks: usize },
    /// 所有任务完成，PM/Advisor 审查中
    Reviewing,
    /// 成功完成
    Completed { summary: String },
    /// 失败
    Failed { reason: String },
}

impl TeamStatus {
    /// 验证状态转换合法性
    pub fn can_transition_to(&self, next: &TeamStatus) -> bool {
        matches!(
            (self, next),
            (TeamStatus::Created, TeamStatus::Planning)
            | (TeamStatus::Planning, TeamStatus::Executing { .. })
            | (TeamStatus::Planning, TeamStatus::Failed { .. })
            | (TeamStatus::Executing { .. }, TeamStatus::Reviewing)
            | (TeamStatus::Executing { .. }, TeamStatus::Failed { .. })
            | (TeamStatus::Reviewing, TeamStatus::Completed { .. })
            | (TeamStatus::Reviewing, TeamStatus::Failed { .. })
            // 允许从 Reviewing 回退到 Executing（审查不通过，需要修复）
            | (TeamStatus::Reviewing, TeamStatus::Executing { .. })
        )
    }
}

// ─── Messages ───────────────────────────────────────────────────────────

/// 团队内部消息协议
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TeamMessage {
    /// Leader → Member: 分配任务
    TaskAssign { task: TaskSpec, boundary: SubAgentBoundary },
    /// Member → PM: 任务状态更新
    TaskUpdate { task_id: String, status: TaskStatus },
    /// Member → Advisor: 请求审查
    ReviewRequest { task_id: String, output: serde_json::Value },
    /// Advisor → Member: 审查结果
    ReviewResult { task_id: String, approved: bool, feedback: String },
    /// Any → Leader: 升级处理
    Escalation { from: AgentRole, reason: String, context: serde_json::Value },
    /// PM → All: 依赖阻塞通知
    DependencyBlocked { task_id: String, waiting_on: Vec<String> },
    /// PM → All: 依赖解除通知
    DependencyResolved { task_id: String },
}

// ─── Context Pools ──────────────────────────────────────────────────────

/// 团队共享上下文（所有角色可见）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SharedContext {
    pub goal: String,
    pub task_board: Vec<TaskInstance>,
    pub known_facts: Vec<String>,
    pub artifacts: Vec<Artifact>,
    pub decisions: Vec<Decision>,
}

/// 角色私有上下文
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrivateContext {
    pub role: Option<AgentRole>,
    pub assigned_tasks: Vec<String>,    // task IDs
    pub working_memory: Vec<String>,    // 工作笔记
}

/// 产出物
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub task_id: String,
    pub content: serde_json::Value,
    pub artifact_type: String,          // "code" | "document" | "config" | "analysis"
}

/// 决策记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub description: String,
    pub made_by: AgentRole,
    pub rationale: String,
    pub timestamp: i64,
}

// ─── Events ─────────────────────────────────────────────────────────────

/// 团队事件（通过 broadcast channel 发送）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TeamEvent {
    TeamCreated { team_id: String, goal: String },
    PlanningStarted { task_count: usize },
    TaskAssigned { task_id: String, role: AgentRole },
    TaskStarted { task_id: String, agent_id: String },
    TaskCompleted { task_id: String, success: bool },
    TaskFailed { task_id: String, error: String },
    ReviewStarted { task_id: String },
    ReviewCompleted { task_id: String, approved: bool },
    Escalated { from: AgentRole, reason: String },
    TeamCompleted { summary: String },
    TeamFailed { reason: String },
}

// ─── Team Session ───────────────────────────────────────────────────────

/// 团队 session（运行时实例）
pub struct TeamSession {
    pub team_id: String,
    pub status: RwLock<TeamStatus>,
    pub shared_ctx: RwLock<SharedContext>,
    pub private_ctxs: RwLock<HashMap<AgentRole, PrivateContext>>,
    /// Agent 间通信信箱（每个角色一个队列）
    pub mailboxes: RwLock<HashMap<AgentRole, Vec<TeamMessage>>>,
    pub event_tx: broadcast::Sender<TeamEvent>,
    pub max_retries: u32,
}

impl TeamSession {
    /// 获取当前状态
    pub async fn status(&self) -> TeamStatus {
        self.status.read().await.clone()
    }

    /// 状态转换（带合法性验证）
    pub async fn transition_to(&self, next: TeamStatus) -> Result<(), KernelError> {
        let mut status = self.status.write().await;
        if !status.can_transition_to(&next) {
            return Err(KernelError::Other(format!(
                "invalid transition: {:?} → {:?}", *status, next
            )));
        }
        *status = next;
        Ok(())
    }

    /// 添加任务到看板
    pub async fn add_task(&self, task: TaskSpec) {
        let instance = TaskInstance {
            spec: task,
            status: TaskStatus::Pending,
            assigned_to: None,
            result: None,
        };
        self.shared_ctx.write().await.task_board.push(instance);
    }

    /// 更新任务状态
    pub async fn update_task_status(&self, task_id: &str, status: TaskStatus) -> Result<(), KernelError> {
        let mut ctx = self.shared_ctx.write().await;
        let task = ctx.task_board.iter_mut()
            .find(|t| t.spec.id == task_id)
            .ok_or_else(|| KernelError::Other(format!("task not found: {task_id}")))?;
        task.status = status;
        Ok(())
    }

    /// 获取就绪任务（依赖已完成 + 未分配）
    pub async fn ready_tasks(&self) -> Vec<TaskSpec> {
        let ctx = self.shared_ctx.read().await;
        let completed_ids: Vec<&str> = ctx.task_board.iter()
            .filter(|t| matches!(t.status, TaskStatus::Completed { .. }))
            .map(|t| t.spec.id.as_str())
            .collect();

        ctx.task_board.iter()
            .filter(|t| matches!(t.status, TaskStatus::Pending))
            .filter(|t| t.spec.depends_on.iter().all(|dep| completed_ids.contains(&dep.as_str())))
            .map(|t| t.spec.clone())
            .collect()
    }

    /// 按角色分组获取就绪任务
    pub async fn ready_tasks_by_role(&self) -> HashMap<AgentRole, Vec<TaskSpec>> {
        let tasks = self.ready_tasks().await;
        let mut by_role: HashMap<AgentRole, Vec<TaskSpec>> = HashMap::new();
        for task in tasks {
            let role = task.required_role.clone().unwrap_or(AgentRole::Member);
            by_role.entry(role).or_default().push(task);
        }
        by_role
    }

    /// 记录 artifact
    pub async fn add_artifact(&self, artifact: Artifact) {
        self.shared_ctx.write().await.artifacts.push(artifact);
    }

    /// 记录决策
    pub async fn add_decision(&self, decision: Decision) {
        self.shared_ctx.write().await.decisions.push(decision);
    }

    /// 发送事件
    pub fn emit(&self, event: TeamEvent) {
        let _ = self.event_tx.send(event);
    }

    /// 发送消息到指定角色的信箱
    pub async fn send_message(&self, to: &AgentRole, msg: TeamMessage) {
        let mut mailboxes = self.mailboxes.write().await;
        mailboxes.entry(to.clone()).or_default().push(msg);
    }

    /// 读取指定角色的信箱（消费式：取出后清空）
    pub async fn recv_messages(&self, role: &AgentRole) -> Vec<TeamMessage> {
        let mut mailboxes = self.mailboxes.write().await;
        mailboxes.remove(role).unwrap_or_default()
    }

    /// 查看指定角色是否有未读消息
    pub async fn has_pending_messages(&self, role: &AgentRole) -> bool {
        let mailboxes = self.mailboxes.read().await;
        mailboxes.get(role).map(|m| !m.is_empty()).unwrap_or(false)
    }

    /// 广播消息到所有角色
    pub async fn broadcast_message(&self, msg: TeamMessage) {
        let roles: Vec<AgentRole> = self.private_ctxs.read().await.keys().cloned().collect();
        let mut mailboxes = self.mailboxes.write().await;
        for role in roles {
            mailboxes.entry(role).or_default().push(msg.clone());
        }
    }

    /// 获取全部任务实例的快照（用于进度展示）
    ///
    /// ## 引用关系
    /// - 消费方: send_team_message (abacus-cli) 构建 TeamTaskInfo
    pub async fn list_tasks(&self) -> Vec<TaskInstance> {
        self.shared_ctx.read().await.task_board.clone()
    }

    /// 所有任务是否完成
    pub async fn all_tasks_done(&self) -> bool {
        let ctx = self.shared_ctx.read().await;
        ctx.task_board.iter().all(|t| {
            matches!(t.status, TaskStatus::Completed { .. } | TaskStatus::Failed { .. })
        })
    }

    /// 统计信息
    pub async fn stats(&self) -> (usize, usize, usize) {
        let ctx = self.shared_ctx.read().await;
        let total = ctx.task_board.len();
        let completed = ctx.task_board.iter()
            .filter(|t| matches!(t.status, TaskStatus::Completed { .. }))
            .count();
        let failed = ctx.task_board.iter()
            .filter(|t| matches!(t.status, TaskStatus::Failed { .. }))
            .count();
        (total, completed, failed)
    }

    /// Execute a single task through CoreLoop (Mode 2 bridge).
    ///
    /// Creates an isolated session for the SubAgent, runs process_turn,
    /// and returns the result. The task description is used as user input.
    ///
    /// ## Arguments
    /// - `core`: shared CoreLoop reference
    /// - `task`: the task to execute
    /// - `role`: the role executing this task (for prompt context)
    ///
    /// ## Returns
    /// Ok(response) on success, Err on failure
    pub async fn execute_task_with_core(
        &self,
        core: &abacus_core::core::CoreLoop,
        task: &TaskSpec,
        role: &AgentRole,
    ) -> Result<String, KernelError> {
        use tokio::sync::RwLock as TokioRwLock;
        use abacus_core::core::SessionState;

        // Create isolated session for this task
        let session_id = format!("team_{}_{}", self.team_id, task.id);
        let session = SessionState::new(&session_id);
        core.register_session_context_tools(&session).await;
        let session = TokioRwLock::new(session);

        // Build prompt from task context with role-specific framing
        let goal = { self.shared_ctx.read().await.goal.clone() };
        let role_context = match role {
            AgentRole::Leader => "You are the team Leader. Focus on architecture, decisions, and quality.",
            AgentRole::PM => "You are the Project Manager. Focus on organization, dependencies, and review.",
            AgentRole::Advisor => "You are an Advisor. Provide expert guidance and recommendations.",
            AgentRole::Member => "You are a team Member. Execute the task as specified.",
        };
        let prompt = format!(
            "You are executing a subtask in a team workflow.\n\
             Role: {} ({})\n\
             Team goal: {}\n\
             Your task: {}\n\
             Task description: {}\n\
             Complete this task and report the result.",
            role, role_context, goal, task.id, task.description
        );

        // Update status to Running
        self.update_task_status(&task.id, TaskStatus::Running {
            agent_id: format!("{}_{}", role, task.id),
        }).await?;

        // Execute
        let result = core.process_turn(&prompt, &session).await?;

        // Update task status
        self.update_task_status(&task.id, TaskStatus::Completed {
            result: serde_json::json!({"response": result.response, "role": role.to_string()}),
        }).await?;

        self.emit(TeamEvent::TaskCompleted {
            task_id: task.id.clone(),
            success: true,
        });

        Ok(result.response)
    }

    /// 流式版本: SubAgent 的 thinking/tool/text 实时流入调用方提供的 stream_tx。
    /// 用于 Team 模式主消息区实时展示每个 SubAgent 工作过程。
    ///
    /// ## 引用关系
    /// - 调用方: send_team_message (abacus-cli/src/tui/api/mod.rs) Phase 2
    /// - 内部调用: CoreLoop::process_turn_streaming
    ///
    /// ## 生命周期
    /// - stream_tx 由调用方持有，本方法仅 clone 使用
    /// - session 为 task 级隔离，方法返回后即释放
    pub async fn execute_task_with_core_streaming(
        &self,
        core: &abacus_core::core::CoreLoop,
        task: &TaskSpec,
        role: &AgentRole,
        stream_tx: tokio::sync::mpsc::UnboundedSender<abacus_core::llm::stream::StreamChunk>,
    ) -> Result<String, KernelError> {
        use tokio::sync::RwLock as TokioRwLock;
        use abacus_core::core::SessionState;

        // Create isolated session for this task
        let session_id = format!("team_{}_{}", self.team_id, task.id);
        let session = SessionState::new(&session_id);
        core.register_session_context_tools(&session).await;
        let session = TokioRwLock::new(session);

        // Build prompt from task context with role-specific framing
        let goal = { self.shared_ctx.read().await.goal.clone() };
        let role_context = match role {
            AgentRole::Leader => "You are the team Leader. Focus on architecture, decisions, and quality.",
            AgentRole::PM => "You are the Project Manager. Focus on organization, dependencies, and review.",
            AgentRole::Advisor => "You are an Advisor. Provide expert guidance and recommendations.",
            AgentRole::Member => "You are a team Member. Execute the task as specified.",
        };
        let prompt = format!(
            "You are executing a subtask in a team workflow.\n\
             Role: {} ({})\n\
             Team goal: {}\n\
             Your task: {}\n\
             Task description: {}\n\
             Complete this task and report the result.",
            role, role_context, goal, task.id, task.description
        );

        // Update status to Running
        self.update_task_status(&task.id, TaskStatus::Running {
            agent_id: format!("{}_{}", role, task.id),
        }).await?;

        // Execute with streaming — agent output flows into caller's stream_tx
        let result = core.process_turn_streaming(&prompt, &session, stream_tx).await?;

        // Update task status
        self.update_task_status(&task.id, TaskStatus::Completed {
            result: serde_json::json!({"response": result.response, "role": role.to_string()}),
        }).await?;

        self.emit(TeamEvent::TaskCompleted {
            task_id: task.id.clone(),
            success: true,
        });

        Ok(result.response)
    }

    /// Execute all ready tasks, dispatching by role.
    /// Tasks are executed role-by-role; within each role, tasks run sequentially.
    pub async fn execute_ready_tasks(
        &self,
        core: &abacus_core::core::CoreLoop,
    ) -> Result<Vec<(String, String)>, KernelError> {
        let by_role = self.ready_tasks_by_role().await;
        let mut all_results = Vec::new();

        // Execute each role's tasks sequentially
        for (role, tasks) in by_role {
            for task in &tasks {
                match self.execute_task_with_core(core, task, &role).await {
                    Ok(r) => all_results.push((task.id.clone(), r)),
                    Err(e) => {
                        self.update_task_status(&task.id, TaskStatus::Failed {
                            error: e.to_string(),
                            retries: 0,
                        }).await?;
                        // Continue with other tasks instead of failing entirely
                        tracing::warn!("Task {} failed: {}", task.id, e);
                    }
                }
            }
        }

        Ok(all_results)
    }
}

// ─── Team Builder ───────────────────────────────────────────────────────

/// 构建器模式创建 TeamSession
pub struct TeamBuilder {
    team_id: String,
    goal: String,
    roles: Vec<AgentRole>,
    tasks: Vec<TaskSpec>,
    max_retries: u32,
}

impl TeamBuilder {
    pub fn new(team_id: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            team_id: team_id.into(),
            goal: goal.into(),
            roles: vec![AgentRole::Leader],
            tasks: Vec::new(),
            max_retries: 2,
        }
    }

    pub fn with_role(mut self, role: AgentRole) -> Self {
        if !self.roles.contains(&role) {
            self.roles.push(role);
        }
        self
    }

    pub fn with_task(mut self, task: TaskSpec) -> Self {
        self.tasks.push(task);
        self
    }

    pub fn max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    pub fn build(self) -> TeamSession {
        let (event_tx, _) = broadcast::channel(64);

        let mut shared_ctx = SharedContext {
            goal: self.goal.clone(),
            ..Default::default()
        };

        // 预填充 task board
        for task in &self.tasks {
            shared_ctx.task_board.push(TaskInstance {
                spec: task.clone(),
                status: TaskStatus::Pending,
                assigned_to: None,
                result: None,
            });
        }

        let mut private_ctxs = HashMap::new();
        for role in &self.roles {
            private_ctxs.insert(role.clone(), PrivateContext {
                role: Some(role.clone()),
                ..Default::default()
            });
        }

        TeamSession {
            team_id: self.team_id,
            status: RwLock::new(TeamStatus::Created),
            shared_ctx: RwLock::new(shared_ctx),
            private_ctxs: RwLock::new(private_ctxs),
            mailboxes: RwLock::new(HashMap::new()),
            event_tx,
            max_retries: self.max_retries,
        }
    }
}

// ─── Team Manager ───────────────────────────────────────────────────────

/// 团队管理器（持有所有 session）
pub struct TeamManager {
    sessions: RwLock<HashMap<String, Arc<TeamSession>>>,
}

impl TeamManager {
    pub fn new() -> Self {
        Self { sessions: RwLock::new(HashMap::new()) }
    }

    /// 注册一个已构建的 TeamSession
    pub async fn register(&self, session: TeamSession) -> Arc<TeamSession> {
        let id = session.team_id.clone();
        let arc = Arc::new(session);
        self.sessions.write().await.insert(id, arc.clone());
        arc
    }

    /// 获取 session
    pub async fn get(&self, team_id: &str) -> Option<Arc<TeamSession>> {
        self.sessions.read().await.get(team_id).cloned()
    }

    /// 列出所有 session
    pub async fn list(&self) -> Vec<String> {
        self.sessions.read().await.keys().cloned().collect()
    }

    /// 移除已完成的 session
    pub async fn remove(&self, team_id: &str) -> bool {
        self.sessions.write().await.remove(team_id).is_some()
    }
}

impl Default for TeamManager {
    fn default() -> Self { Self::new() }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_team_status_transitions() {
        let session = TeamBuilder::new("t1", "test goal").build();

        // Created → Planning: valid
        assert!(session.transition_to(TeamStatus::Planning).await.is_ok());

        // Planning → Executing: valid
        assert!(session.transition_to(TeamStatus::Executing { active_tasks: 2, completed_tasks: 0 }).await.is_ok());

        // Executing → Reviewing: valid
        assert!(session.transition_to(TeamStatus::Reviewing).await.is_ok());

        // Reviewing → Completed: valid
        assert!(session.transition_to(TeamStatus::Completed { summary: "done".into() }).await.is_ok());
    }

    #[tokio::test]
    async fn test_invalid_transition() {
        let session = TeamBuilder::new("t2", "test").build();
        // Created → Completed: invalid (skip Planning/Executing)
        let result = session.transition_to(TeamStatus::Completed { summary: "x".into() }).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ready_tasks_dependency() {
        let task_a = TaskSpec {
            id: "a".into(), description: "task A".into(),
            required_capabilities: vec![], allowed_tools: vec![],
            priority: 0, depends_on: vec![], required_role: None,
        };
        let task_b = TaskSpec {
            id: "b".into(), description: "task B".into(),
            required_capabilities: vec![], allowed_tools: vec![],
            priority: 1, depends_on: vec!["a".into()], required_role: None,
        };

        let session = TeamBuilder::new("t3", "dep test")
            .with_task(task_a)
            .with_task(task_b)
            .build();

        // 初始：只有 A 就绪（B 依赖 A）
        let ready = session.ready_tasks().await;
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "a");

        // A 完成后，B 就绪
        session.update_task_status("a", TaskStatus::Completed {
            result: serde_json::json!({"ok": true})
        }).await.unwrap();

        let ready = session.ready_tasks().await;
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "b");
    }

    #[tokio::test]
    async fn test_team_builder() {
        let session = TeamBuilder::new("t4", "build test")
            .with_role(AgentRole::PM)
            .with_role(AgentRole::Member)
            .max_retries(3)
            .build();

        assert_eq!(session.team_id, "t4");
        assert_eq!(session.max_retries, 3);

        let status = session.status().await;
        assert_eq!(status, TeamStatus::Created);
    }

    #[tokio::test]
    async fn test_team_events() {
        let session = TeamBuilder::new("t5", "event test").build();
        let mut rx = session.event_tx.subscribe();

        session.emit(TeamEvent::TeamCreated {
            team_id: "t5".into(),
            goal: "event test".into(),
        });

        let event = rx.recv().await.unwrap();
        assert!(matches!(event, TeamEvent::TeamCreated { .. }));
    }

    #[tokio::test]
    async fn test_team_manager() {
        let manager = TeamManager::new();
        let session = TeamBuilder::new("tm1", "manager test").build();

        let arc = manager.register(session).await;
        assert_eq!(arc.team_id, "tm1");

        let found = manager.get("tm1").await;
        assert!(found.is_some());

        let list = manager.list().await;
        assert_eq!(list.len(), 1);

        manager.remove("tm1").await;
        assert!(manager.get("tm1").await.is_none());
    }

    #[tokio::test]
    async fn test_message_send_recv() {
        let session = TeamBuilder::new("t6", "comm test")
            .with_role(AgentRole::PM)
            .with_role(AgentRole::Member)
            .build();

        // Leader 发消息给 Member
        session.send_message(&AgentRole::Member, TeamMessage::TaskAssign {
            task: TaskSpec {
                id: "task_1".into(), description: "do thing".into(),
                required_capabilities: vec![], allowed_tools: vec![],
                priority: 0, depends_on: vec![], required_role: None,
            },
            boundary: crate::subagent::SubAgentBoundary::default(),
        }).await;

        // Member 有未读消息
        assert!(session.has_pending_messages(&AgentRole::Member).await);
        assert!(!session.has_pending_messages(&AgentRole::PM).await);

        // Member 读取（消费式）
        let msgs = session.recv_messages(&AgentRole::Member).await;
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0], TeamMessage::TaskAssign { .. }));

        // 读取后信箱为空
        assert!(!session.has_pending_messages(&AgentRole::Member).await);
    }

    #[tokio::test]
    async fn test_broadcast_message() {
        let session = TeamBuilder::new("t7", "broadcast test")
            .with_role(AgentRole::PM)
            .with_role(AgentRole::Member)
            .with_role(AgentRole::Advisor)
            .build();

        // 广播消息
        session.broadcast_message(TeamMessage::Escalation {
            from: AgentRole::Member,
            reason: "blocked".into(),
            context: serde_json::json!({"task": "x"}),
        }).await;

        // 所有角色都收到
        assert!(session.has_pending_messages(&AgentRole::Leader).await);
        assert!(session.has_pending_messages(&AgentRole::PM).await);
        assert!(session.has_pending_messages(&AgentRole::Member).await);
        assert!(session.has_pending_messages(&AgentRole::Advisor).await);
    }

    /// 模拟 Mode 2 完整运行时场景：
    /// Leader 分解 → 分配给 Member → Member 执行 → 报告结果 → PM 审查 → 完成
    #[tokio::test]
    async fn test_mode2_runtime_simulation() {
        let task_a = TaskSpec {
            id: "impl_auth".into(), description: "实现登录功能".into(),
            required_capabilities: vec!["rust".into()],
            allowed_tools: vec!["filengine_fs_read".into(), "filengine_fs_write".into()],
            priority: 0, depends_on: vec![], required_role: Some(AgentRole::Member),
        };
        let task_b = TaskSpec {
            id: "test_auth".into(), description: "编写登录测试".into(),
            required_capabilities: vec!["testing".into()],
            allowed_tools: vec!["filengine_fs_read".into(), "filengine_fs_write".into()],
            priority: 1, depends_on: vec!["impl_auth".into()], required_role: Some(AgentRole::PM),
        };

        let session = TeamBuilder::new("team_sim", "实现用户认证系统")
            .with_role(AgentRole::PM)
            .with_role(AgentRole::Member)
            .with_task(task_a.clone())
            .with_task(task_b.clone())
            .build();

        let mut rx = session.event_tx.subscribe();

        // === Phase 1: Planning ===
        session.transition_to(TeamStatus::Planning).await.unwrap();

        // Leader 分配 task_a 给 Member
        session.send_message(&AgentRole::Member, TeamMessage::TaskAssign {
            task: task_a.clone(),
            boundary: crate::subagent::SubAgentBoundary::default(),
        }).await;
        session.update_task_status("impl_auth", TaskStatus::Assigned {
            role: AgentRole::Member, agent_id: "sa_1".into(),
        }).await.unwrap();

        session.emit(TeamEvent::TaskAssigned {
            task_id: "impl_auth".into(), role: AgentRole::Member,
        });

        // === Phase 2: Executing ===
        session.transition_to(TeamStatus::Executing {
            active_tasks: 1, completed_tasks: 0,
        }).await.unwrap();

        // Member 读取消息并执行
        let msgs = session.recv_messages(&AgentRole::Member).await;
        assert_eq!(msgs.len(), 1);

        // Member 完成 task_a
        session.update_task_status("impl_auth", TaskStatus::Completed {
            result: serde_json::json!({"files_created": ["src/auth.rs"]}),
        }).await.unwrap();
        session.add_artifact(Artifact {
            id: "art_1".into(), task_id: "impl_auth".into(),
            content: serde_json::json!({"file": "src/auth.rs", "lines": 120}),
            artifact_type: "code".into(),
        }).await;
        session.emit(TeamEvent::TaskCompleted { task_id: "impl_auth".into(), success: true });

        // task_b 现在就绪（依赖 impl_auth 已完成）
        let ready = session.ready_tasks().await;
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "test_auth");

        // PM 发送依赖解除通知
        session.send_message(&AgentRole::Member, TeamMessage::DependencyResolved {
            task_id: "test_auth".into(),
        }).await;

        // Member 执行 task_b
        session.update_task_status("test_auth", TaskStatus::Completed {
            result: serde_json::json!({"tests_passed": 5}),
        }).await.unwrap();
        session.emit(TeamEvent::TaskCompleted { task_id: "test_auth".into(), success: true });

        // === Phase 3: Reviewing ===
        assert!(session.all_tasks_done().await);
        session.transition_to(TeamStatus::Reviewing).await.unwrap();

        // PM 审查通过
        session.send_message(&AgentRole::Leader, TeamMessage::ReviewResult {
            task_id: "impl_auth".into(), approved: true, feedback: "LGTM".into(),
        }).await;

        // === Phase 4: Complete ===
        session.transition_to(TeamStatus::Completed {
            summary: "用户认证系统实现完成，5 个测试通过".into(),
        }).await.unwrap();
        session.emit(TeamEvent::TeamCompleted {
            summary: "用户认证系统实现完成".into(),
        });

        // 验证统计
        let (total, completed, failed) = session.stats().await;
        assert_eq!(total, 2);
        assert_eq!(completed, 2);
        assert_eq!(failed, 0);

        // 验证事件流
        let event = rx.recv().await.unwrap();
        assert!(matches!(event, TeamEvent::TaskAssigned { .. }));
    }

    #[tokio::test]
    async fn test_ready_tasks_by_role() {
        let task_leader = TaskSpec {
            id: "plan".into(), description: "Plan architecture".into(),
            required_capabilities: vec![], allowed_tools: vec![],
            priority: 0, depends_on: vec![], required_role: Some(AgentRole::Leader),
        };
        let task_member_a = TaskSpec {
            id: "impl_a".into(), description: "Implement feature A".into(),
            required_capabilities: vec![], allowed_tools: vec![],
            priority: 1, depends_on: vec![], required_role: Some(AgentRole::Member),
        };
        let task_member_b = TaskSpec {
            id: "impl_b".into(), description: "Implement feature B".into(),
            required_capabilities: vec![], allowed_tools: vec![],
            priority: 1, depends_on: vec![], required_role: Some(AgentRole::Member),
        };
        let task_any = TaskSpec {
            id: "docs".into(), description: "Write docs".into(),
            required_capabilities: vec![], allowed_tools: vec![],
            priority: 2, depends_on: vec![], required_role: None,
        };

        let session = TeamBuilder::new("t_role", "role dispatch test")
            .with_role(AgentRole::Leader)
            .with_role(AgentRole::PM)
            .with_role(AgentRole::Member)
            .with_task(task_leader)
            .with_task(task_member_a)
            .with_task(task_member_b)
            .with_task(task_any)
            .build();

        let by_role = session.ready_tasks_by_role().await;

        // Leader tasks
        let leader_tasks = by_role.get(&AgentRole::Leader).unwrap();
        assert_eq!(leader_tasks.len(), 1);
        assert_eq!(leader_tasks[0].id, "plan");

        // Member tasks (2 explicit + 1 default None → Member)
        let member_tasks = by_role.get(&AgentRole::Member).unwrap();
        assert_eq!(member_tasks.len(), 3);

        // PM has no tasks
        assert!(!by_role.contains_key(&AgentRole::PM));
    }
}
