//! # Specialist — 领域专家容器
//!
//! ## 场景
//! AgentMeeting 模式 (Mode 3) 中，Specialist 代表一个独立的领域专家，
//! 与 SubAgent (Mode 2) 的关键区别：
//! - SubAgent = 执行容器，由 SubAgentBoundary(steps/tokens/tools) 定义
//! - Specialist = 领域专家，由 Specialty(domain/guide/anti_pattern) 定义
//!
//! ## 依赖链
//! ```text
//! abacus-types
//!   └── abacus-orchestrator/src/team (AgentRole)
//!         └── abacus-orchestrator/src/specialist ← 本模块
//!               └── abacus-orchestrator/src/meeting (MeetingSession)
//! ```
//!
//! ## 引用关系
//! - `SpecialistRegistry` 由 `MeetingSession` 持有（创建实例）
//! - `SpecialistOpinion` 由 `MeetingSession.timeline` 持久化
//! - `SpecialistInstance` 由 `MeetingRouter` 读取（路由匹配）
//! - `ThinkingStep`/`ToolCallRecord` 由 `MeetingEvent` → Dashboard 消费
//!
//! ## 边界
//! - SpecialistInstance 禁止跨 MeetingSession 共享
//! - SpecialistStatus 转换有强制校验 (can_transition_to)
//! - SpecialistRegistry 线程安全，支持并发访问

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tokio::sync::RwLock;

use crate::team::AgentRole;

// ─── 错误类型 ───────────────────────────────────────────────────────────

#[derive(Error, Debug, Clone, PartialEq)]
pub enum SpecialistError {
    #[error("Specialist {0} 未注册")]
    NotRegistered(String),

    #[error("无效状态转换: {from} → {to}")]
    InvalidTransition { from: SpecialistStatus, to: SpecialistStatus },

    #[error("已达最大参与人数 ({0})")]
    MaxParticipants(u32),

    #[error("Specialist 超出参与限制: {detail}")]
    EngagementLimitExceeded { detail: String },

    #[error("{0}")]
    Other(String),
}

// ─── 核心标识 ───────────────────────────────────────────────────────────

/// Specialist ID — 语义化标识
///
/// 格式: `sp-{registration_id}`
/// 示例: `sp-coder`, `sp-reviewer`, `sp-architect`
///
/// ## 与 SubAgent ID 的区别
/// SubAgent ID: `agent_N`（递增数字，无语义）
/// Specialist ID: `sp-{domain}`（语义化，用户可预测）
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SpecialistId(pub String);

// ─── 参与度限制 ─────────────────────────────────────────────────────────

/// 参与度限制
///
/// ## 与 SubAgentBoundary 的语义区别
/// SubAgentBoundary: 控制"能调什么工具、能用多少 token"（执行限制）
/// EngagementLimit: 控制"在会议中能参与到什么程度"（参与限制）
///
/// ## 边界条件
/// - `max_speeches_per_round`: 防刷屏上限
/// - `max_thinking_tokens`: 超限后截断 thinking 保留结论
/// - `min_confidence < 0.0` 或 `> 1.0`: clamp 到 [0,1]
/// - `response_timeout_secs = 0`: 不允许超时（等价于必须同步返回）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngagementLimit {
    pub max_speeches_per_round: u32,
    pub max_thinking_tokens: usize,
    pub max_tool_calls_per_think: u32,
    pub min_confidence: f64,
    pub response_timeout_secs: u64,
    /// Phase 4：per-specialist thinking 意图覆盖。
    /// 例：研究员可设 `thinking: "high"`，文案编辑可设 `thinking: "low"`。
    /// 缺省 → 走全局 core.thinking。
    /// YAML 字符串：off/adaptive/minimal/low/medium/high/max/xhigh/<整数>
    #[serde(default)]
    pub thinking: Option<String>,
}

impl EngagementLimit {
    pub fn validate(&self) -> Result<(), SpecialistError> {
        if !(0.0..=1.0).contains(&self.min_confidence) {
            return Err(SpecialistError::EngagementLimitExceeded {
                detail: "min_confidence 必须在 [0,1] 范围内".into(),
            });
        }
        Ok(())
    }

    /// Phase 4：解析 thinking 字符串到 ThinkingIntent。
    /// 失败（含 None / 无法解析）返回 None，调用方走全局 default。
    pub fn parse_thinking_intent(&self) -> Option<abacus_types::ThinkingIntent> {
        self.thinking.as_deref()
            .and_then(abacus_types::ThinkingIntent::from_str_loose)
    }
}

impl Default for EngagementLimit {
    fn default() -> Self {
        Self {
            max_speeches_per_round: 3,
            max_thinking_tokens: 4096,
            max_tool_calls_per_think: 5,
            min_confidence: 0.3,
            response_timeout_secs: 120,
            thinking: None,
        }
    }
}

// ─── 专业定义 ───────────────────────────────────────────────────────────

/// 专业定义 — Specialist 区别于 SubAgent 的核心结构
///
/// SubAgent 由 SubAgentBoundary(steps/tokens/tools) 定义
/// Specialist 由 Specialty(domain/guide/anti_pattern) 定义
///
/// ## 字段说明
/// - `domain`: 领域名称，如 "code_review", "ux_design", "architecture"
/// - `guide_strategy`: Expert GuideStrategy 文本，注入 Specialist 独立 prompt 链
/// - `anti_pattern`: Expert AntiPattern 文本，由 MeetingHarness.post_check 验证
/// - `knowledge_mounts`: 领域知识挂载点，DynamicInjector 按此加载
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct Specialty {
    pub domain: String,
    pub description: String,
    pub key_capabilities: Vec<String>,
    pub hint_tags: Vec<String>,
    pub expert_ref: Option<String>,
    pub guide_strategy: String,
    pub anti_pattern: String,
    pub knowledge_mounts: Vec<String>,
    pub engagement: EngagementLimit,
}


impl Specialty {
    /// 验证 Specialty 配置是否合法
    pub fn validate(&self) -> Result<(), SpecialistError> {
        if self.domain.is_empty() {
            return Err(SpecialistError::Other("domain 不能为空".into()));
        }
        self.engagement.validate()?;
        Ok(())
    }
}

// ─── 推理过程记录 ────────────────────────────────────────────────────────

/// 推理中间步骤 — → Dashboard Thinking 页
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingStep {
    pub index: u32,
    pub thought: String,
    pub timestamp_ms: i64,
}

/// 工具调用记录 — → Dashboard ToolCalls 页
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub tool_id: String,
    pub arguments: serde_json::Value,
    pub status: ToolCallStatus,
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolCallStatus {
    Running,
    Success,
    Error(String),
}

// ─── 推理结论 ────────────────────────────────────────────────────────────

/// Specialist 推理结论
///
/// ## 与 SubAgentResult 的区别
/// SubAgentResult: 执行结果 (success/output/tokens/duration)
/// SpecialistOpinion: 推理结论 (conclusion/confidence/suggestions)
///
/// ## 输出路径
/// - `conclusion` → 主消息区展示
/// - `reasoning_summary` → Dashboard Thinking 页
/// - `tool_evidence` → Dashboard ToolCalls 页
/// - `suggestions` → 主持人路由参考
/// - `requires_attention` → 跨 Specialist 引用（如 Coder 结论需 Reviewer 确认）
///
/// ## 边界
/// - `auto_approve=false && host_review_required=true` → 需主持人确认后再展示
/// - `confidence < engagement.min_confidence` → 不自动展示，标记低置信
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialistOpinion {
    pub specialist_id: SpecialistId,
    pub turn: u32,
    pub conclusion: String,
    pub confidence: f64,
    pub reasoning_summary: String,
    pub tool_evidence: Vec<ToolCallRecord>,
    pub suggestions: Vec<String>,
    pub requires_attention: Vec<SpecialistId>,
    pub auto_approve: bool,
    pub host_review_required: bool,
}

// ─── 运行时状态 ─────────────────────────────────────────────────────────

/// Specialist 运行时状态
///
/// ## 与 SubAgentStatus 的区别
/// SubAgentStatus: Pending → Running → Completed/Failed（单向终结）
/// SpecialistStatus: Registered ↔ Invited ↔ Listening ↔ Thinking ↔ Speaking（循环）
///
/// ## 状态流转
/// ```text
/// Registered ──invite()──→ Invited ──confirm()──→ Listening
///                                                    │
///   ┌────────────────────────────────────────────────┤
///   │                                                │
///   │     @mention/路由                                │
///   │        ↓                                       │
///   │    Thinking ──产出结论──→ Speaking                │
///   │                                │                │
///   │                         主持人发布结论            │
///   │                                │                │
///   └── AwaitingInput ←──────────────┘                │
///         │                                           │
///         用户继续追问 ─────────────────────────────────┘
///
/// Error ──→ 主持人接管（恢复 Listening 或移除）
/// Completed / Inactive → 终结状态
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SpecialistStatus {
    Registered,
    Invited,
    Listening,
    Thinking,
    Speaking,
    AwaitingInput,
    Completed,
    Inactive,
    #[serde(skip)]
    Error(String),
}

impl fmt::Display for SpecialistStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecialistStatus::Registered => write!(f, "Registered"),
            SpecialistStatus::Invited => write!(f, "Invited"),
            SpecialistStatus::Listening => write!(f, "Listening"),
            SpecialistStatus::Thinking => write!(f, "Thinking"),
            SpecialistStatus::Speaking => write!(f, "Speaking"),
            SpecialistStatus::AwaitingInput => write!(f, "AwaitingInput"),
            SpecialistStatus::Completed => write!(f, "Completed"),
            SpecialistStatus::Inactive => write!(f, "Inactive"),
            SpecialistStatus::Error(_) => write!(f, "Error"),
        }
    }
}

impl SpecialistStatus {
    /// 校验状态转换合法性
    ///
    /// ## 允许的转换
    /// - Registered → Invited
    /// - Invited → Listening
    /// - Listening → Thinking | AwaitingInput
    /// - Thinking → Speaking | Error | Listening
    /// - Speaking → AwaitingInput | Listening
    /// - AwaitingInput → Listening | Thinking | Completed
    /// - Error → Listening | Inactive
    /// - Completed / Inactive → 终结，不可再转换
    pub fn can_transition_to(&self, next: &SpecialistStatus) -> bool {
        use SpecialistStatus::*;
        matches!((self, next),
            (Registered, Invited) |
            (Invited, Listening) |
            (Listening, Thinking) | (Listening, AwaitingInput) |
            (Thinking, Speaking) | (Thinking, Error(_)) | (Thinking, Listening) |
            (Speaking, AwaitingInput) | (Speaking, Listening) |
            (AwaitingInput, Listening) | (AwaitingInput, Thinking) | (AwaitingInput, Completed) |
            (Error(_), Inactive) | (Error(_), Listening) |
            (Completed, Listening) |
            (Inactive, Listening)
        )
    }

    pub fn is_active(&self) -> bool {
        matches!(self, SpecialistStatus::Listening
            | SpecialistStatus::Thinking
            | SpecialistStatus::Speaking
            | SpecialistStatus::AwaitingInput)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, SpecialistStatus::Completed | SpecialistStatus::Inactive)
    }
}

// ─── 实例 ───────────────────────────────────────────────────────────────

/// Specialist 实例 — 运行时状态容器
///
/// ## 与 SubAgentInstance 的对比
/// | 字段 | SubAgentInstance | SpecialistInstance |
/// |------|-----------------|-------------------|
/// | ID | agent_N（递增） | sp-{domain}（语义化） |
/// | 核心定义 | SubAgentBoundary | Specialty |
/// | 角色 | 无 | AgentRole (Leader/PM/Advisor/Member) |
/// | 状态机 | 单向线性 | 循环（可多次 Thinking→Speaking）|
///
/// ## 线程安全
/// 实例通过 Arc<RwLock<>> 在 MeetingSession 内部共享
///
/// ## 生命周期
/// MeetingSession 创建 → 邀请 → 参与会议 → 会议结束自动销毁
#[derive(Debug, Clone)]
pub struct SpecialistInstance {
    pub id: SpecialistId,
    pub name: String,
    pub avatar: Option<String>,
    pub role: AgentRole,
    pub specialty: Specialty,
    pub status: SpecialistStatus,
    pub current_turn: u32,
    pub speeches_count: u32,
    pub thinking: Vec<ThinkingStep>,
    pub tool_calls: Vec<ToolCallRecord>,
    /// V0.2: 此 specialist 偏好的模型（从 Registration 复制）
    pub preferred_model: Option<String>,
}

impl SpecialistInstance {
    /// 尝试转换到新状态，失败返回错误
    pub fn try_transition_to(&mut self, next: SpecialistStatus) -> Result<(), SpecialistError> {
        if !self.status.can_transition_to(&next) {
            return Err(SpecialistError::InvalidTransition {
                from: self.status.clone(),
                to: next,
            });
        }
        self.status = next;
        Ok(())
    }

    /// 记录一次发言
    pub fn record_speech(&mut self) {
        self.speeches_count += 1;
        self.current_turn += 1;
    }

    /// 检查是否超过发言限制
    pub fn exceeded_speech_limit(&self, limit: &EngagementLimit) -> bool {
        self.speeches_count >= limit.max_speeches_per_round
    }
}

// ─── 注册配置 ───────────────────────────────────────────────────────────

/// Specialist 注册配置 — 从配置文件加载
///
/// ## YAML 示例
/// ```yaml
/// specialists:
///   - id: coder
///     domain: coding
///     name: "Coder"
///     role: Member
///     model: deepseek-v4-flash
///     guide_strategy: "你是代码实现专家..."
///     anti_pattern: "禁止在未确认时修改生产文件..."
///     capabilities: [code_implementation]
///     tags: ["代码", "编程"]
///     allowed_tools: ["filengine_fs_*"]
///     engagement:
///       max_speeches_per_round: 3
///       max_thinking_tokens: 4096
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialistRegistration {
    pub id: String,
    pub domain: String,
    pub name: String,
    pub role: AgentRole,
    pub model: String,
    pub guide_strategy: String,
    pub anti_pattern: String,
    pub capabilities: Vec<String>,
    pub tags: Vec<String>,
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub engagement: EngagementLimit,
}

impl SpecialistRegistration {
    pub fn to_specialty(&self) -> Specialty {
        Specialty {
            domain: self.domain.clone(),
            description: self.name.clone(),
            key_capabilities: self.capabilities.clone(),
            hint_tags: self.tags.clone(),
            expert_ref: None,
            guide_strategy: self.guide_strategy.clone(),
            anti_pattern: self.anti_pattern.clone(),
            knowledge_mounts: vec![],
            engagement: self.engagement.clone(),
        }
    }
}

// ─── 注册表 ─────────────────────────────────────────────────────────────

/// Specialist 注册表
///
/// ## 并发安全
/// - `registrations`: 启动时一次性写入，之后只读 → 不加锁 (HashMap)
/// - `instances`: 运行时动态创建/销毁 → RwLock 保护
///
/// ## 依赖关系
/// - 被 `MeetingSession` 持有（`crate::meeting`）
/// - 被 `MeetingRouter` 引用（路由时查询注册表）
///
/// ## 生命周期
/// 1. 系统启动时从 Config 加载 → `register()`
/// 2. 会议开始时 → `create_instance()` 创建运行时实例
/// 3. 会议结束时 → 实例自动销毁，注册配置保留（不删除）
pub struct SpecialistRegistry {
    registrations: HashMap<String, SpecialistRegistration>,
    instances: RwLock<HashMap<SpecialistId, SpecialistInstance>>,
    /// 单调递增序列号，用于生成实例 ID 后缀（同名 Specialist 多实例区分）
    seq: AtomicU64,
}

impl SpecialistRegistry {
    pub fn new() -> Self {
        Self {
            registrations: HashMap::new(),
            instances: RwLock::new(HashMap::new()),
            seq: AtomicU64::new(1),
        }
    }

    /// 注册一个 Specialist 类型（系统初始化时调用）
    pub fn register(&mut self, reg: SpecialistRegistration) -> Result<(), SpecialistError> {
        if reg.id.is_empty() {
            return Err(SpecialistError::Other("registration id 不能为空".into()));
        }
        self.registrations.insert(reg.id.clone(), reg);
        Ok(())
    }

    /// List all registered specialist definitions.
    pub fn list_registrations(&self) -> Vec<&SpecialistRegistration> {
        self.registrations.values().collect()
    }

    /// 从 YAML 批量加载注册配置
    pub fn register_all(&mut self, regs: Vec<SpecialistRegistration>) -> Result<(), SpecialistError> {
        for reg in regs {
            self.register(reg)?;
        }
        Ok(())
    }

    pub fn get_registration(&self, id: &str) -> Option<&SpecialistRegistration> {
        self.registrations.get(id)
    }

    /// 创建运行时实例
    ///
    /// ## 流程
    /// 1. 从 registrations 查找注册配置
    /// 2. 校验配置合法性
    /// 3. 创建 SpecialistInstance，初始状态 = Registered
    /// 4. 存入 instances
    ///
    /// ## 线程安全
    /// - 读 registrations: 无需锁（只写一次后只读）
    /// - 写 instances: RwLock write
    pub async fn create_instance(&self, id: &str, role: AgentRole) -> Result<SpecialistInstance, SpecialistError> {
        let reg = self.registrations.get(id)
            .ok_or_else(|| SpecialistError::NotRegistered(id.to_string()))?;

        reg.to_specialty().validate()?;

        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let instance = SpecialistInstance {
            id: SpecialistId(format!("sp-{}-{}", id, seq)),
            name: reg.name.clone(),
            avatar: Some(reg.name.chars().next().unwrap_or('?').to_string()),
            role,
            specialty: reg.to_specialty(),
            status: SpecialistStatus::Registered,
            current_turn: 0,
            speeches_count: 0,
            thinking: vec![],
            tool_calls: vec![],
            preferred_model: if reg.model.is_empty() { None } else { Some(reg.model.clone()) },
        };
        let sp_id = instance.id.clone();
        self.instances.write().await.insert(sp_id, instance.clone());
        Ok(instance)
    }

    pub async fn get_instance(&self, id: &SpecialistId) -> Option<SpecialistInstance> {
        self.instances.read().await.get(id).cloned()
    }

    /// 更新 Specialist 状态（带校验）
    pub async fn update_status(&self, id: &SpecialistId, next: SpecialistStatus) -> Result<SpecialistStatus, SpecialistError> {
        let mut instances = self.instances.write().await;
        let sp = instances.get_mut(id)
            .ok_or_else(|| SpecialistError::NotRegistered(id.0.clone()))?;
        let prev = sp.status.clone();
        sp.try_transition_to(next)?;
        Ok(prev)
    }

    /// 删除实例（会议结束时清理）
    pub async fn remove_instance(&self, id: &SpecialistId) {
        self.instances.write().await.remove(id);
    }

    pub async fn instance_count(&self) -> usize {
        self.instances.read().await.len()
    }
}

impl Default for SpecialistRegistry {
    fn default() -> Self { Self::new() }
}

// ─── 测试 ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_reg(id: &str) -> SpecialistRegistration {
        SpecialistRegistration {
            id: id.into(),
            domain: "test".into(),
            name: id.into(),
            role: AgentRole::Member,
            model: "test".into(),
            guide_strategy: "".into(),
            anti_pattern: "".into(),
            capabilities: vec![],
            tags: vec![],
            allowed_tools: vec![],
            engagement: EngagementLimit::default(),
        }
    }

    #[tokio::test]
    async fn test_register_and_create() {
        let mut registry = SpecialistRegistry::new();
        registry.register(make_reg("coder")).unwrap();
        let instance = registry.create_instance("coder", AgentRole::Member).await.unwrap();
        assert!(instance.id.0.starts_with("sp-coder-"));
        assert_eq!(instance.status, SpecialistStatus::Registered);
    }

    #[tokio::test]
    async fn test_create_unregistered_returns_error() {
        let registry = SpecialistRegistry::new();
        let result = registry.create_instance("nonexistent", AgentRole::Member).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SpecialistError::NotRegistered(_)));
    }

    #[test]
    fn test_status_transition_valid() {
        assert!(SpecialistStatus::Registered.can_transition_to(&SpecialistStatus::Invited));
        assert!(SpecialistStatus::Thinking.can_transition_to(&SpecialistStatus::Speaking));
        assert!(SpecialistStatus::Speaking.can_transition_to(&SpecialistStatus::AwaitingInput));
        assert!(SpecialistStatus::AwaitingInput.can_transition_to(&SpecialistStatus::Thinking));
    }

    #[test]
    fn test_status_transition_invalid() {
        assert!(!SpecialistStatus::Registered.can_transition_to(&SpecialistStatus::Speaking));
        assert!(!SpecialistStatus::Completed.can_transition_to(&SpecialistStatus::Thinking));
        assert!(!SpecialistStatus::Inactive.can_transition_to(&SpecialistStatus::Thinking));
    }

    #[tokio::test]
    async fn test_update_status_validates_transition() {
        let mut registry = SpecialistRegistry::new();
        registry.register(make_reg("coder")).unwrap();
        let instance = registry.create_instance("coder", AgentRole::Member).await.unwrap();

        // Registered → Speaking: invalid
        let result = registry.update_status(&instance.id, SpecialistStatus::Speaking).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SpecialistError::InvalidTransition { .. }));

        // Registered → Invited: valid
        let result = registry.update_status(&instance.id, SpecialistStatus::Invited).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_engagement_limit_validation() {
        let mut limit = EngagementLimit::default();
        assert!(limit.validate().is_ok());

        limit.min_confidence = 1.5;
        assert!(limit.validate().is_err());
    }
}
