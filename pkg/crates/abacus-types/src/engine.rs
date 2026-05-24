//! L2 engine layer types — ToolRegistry, SkillEngine, CapabilityHub
//!
//! Pure data types without runtime dependencies. Runtime implementations
//! live in `abacus-core` crate.

use serde::{Deserialize, Serialize};

// ─── Tool System ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolId(pub String);

impl From<&str> for ToolId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub returns: Option<serde_json::Value>,
    pub security: Option<ToolSecurity>,
    pub cost: Option<ToolCost>,
    /// Phase β-C: 调用示例（参数 + 期望输出）让 LLM 学习正确填参数
    ///
    /// 默认空 Vec——序列化时跳过；填写后 build_tool_definitions 把示例
    /// 拼到 description 末尾。LLM 看到 [example: read('/etc/host')] 会显著提升首次调用准确率。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<ToolExample>,
    /// Phase β-D: 该工具适用的任务类型白名单
    ///
    /// `None`（默认）= 所有任务都可见；`Some(list)` = 仅当 task_kind 命中才暴露给 LLM。
    /// 配合 CoreConfig.task_kind_routing_enabled 启用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applicable_task_kinds: Option<Vec<String>>,
    /// Phase β-G: 是否幂等（多次调用同参数 → 同结果，无副作用）
    ///
    /// 默认 `false`（保守）。pipeline 检查多个 idempotent 工具时可以并行执行
    /// 加速 latency。filengine_fs_read / db_read_records 等读操作典型 idempotent；
    /// filengine_fs_write / filengine_bash_exec 等写操作必须 false。
    #[serde(default)]
    pub idempotent: bool,
}

/// Phase β-C: 工具调用示例
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExample {
    /// 示例描述（如「读取项目根目录的配置文件」）
    pub description: String,
    /// 参数 JSON
    pub params: serde_json::Value,
    /// 可选：期望输出（结构示意）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_output: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSecurity {
    pub allowed_paths: Option<Vec<String>>,
    pub max_size_mb: Option<u32>,
    pub confirm_required: bool,
    pub needs_sandbox: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCost {
    pub tokens: u32,
    pub latency: String,
    pub risk: String,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerPattern(pub Vec<String>);

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolProvider {
    BuiltIn,
    Mcp { server_id: String },
    Plugin { plugin_id: String },
    Skill { skill_id: String },
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolState {
    Registered,
    Loaded,
    Active,
    Cooling,
    Disabled,
}

impl ToolState {
    /// 验证状态转换合法性。
    ///
    /// ## 合法转换
    /// ```text
    /// Registered → Loaded (加载完成)
    /// Loaded → Active (被调用)
    /// Active → Loaded (调用结束)
    /// Active → Cooling (触发 cooldown)
    /// Cooling → Loaded (cooldown 结束)
    /// Loaded → Disabled (手动禁用)
    /// Active → Disabled (手动禁用)
    /// Cooling → Disabled (手动禁用)
    /// Disabled → Loaded (重新启用)
    /// Registered → Disabled (加载失败)
    /// ```
    pub fn can_transition_to(&self, next: &ToolState) -> bool {
        matches!(
            (self, next),
            (ToolState::Registered, ToolState::Loaded)
            | (ToolState::Registered, ToolState::Disabled)
            | (ToolState::Loaded, ToolState::Active)
            | (ToolState::Active, ToolState::Loaded)
            | (ToolState::Active, ToolState::Cooling)
            | (ToolState::Cooling, ToolState::Loaded)
            | (ToolState::Loaded, ToolState::Disabled)
            | (ToolState::Active, ToolState::Disabled)
            | (ToolState::Cooling, ToolState::Disabled)
            | (ToolState::Disabled, ToolState::Loaded)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolEffectiveness {
    pub tool_id: ToolId,
    pub composite_score: f64,
    pub tier: VisibilityTier,
    pub cooldown_remaining: u32,
    pub blocked_by_env: bool,
    pub insufficient_data: bool,
}

impl Default for ToolEffectiveness {
    fn default() -> Self {
        Self {
            tool_id: ToolId("unknown".into()),
            composite_score: 0.6,
            tier: VisibilityTier::A,
            cooldown_remaining: 0,
            blocked_by_env: false,
            insufficient_data: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VisibilityTier {
    S,
    A,
    B,
    C,
    D,
}

/// Engine-level tool handle (pure data, no runtime handles)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolHandle {
    pub id: ToolId,
    pub schema: ToolSchema,
    pub provider: ToolProvider,
    pub state: ToolState,
    pub effectiveness: ToolEffectiveness,
}

// ─── Skill System ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillId(pub String);

impl From<&str> for SkillId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

impl std::fmt::Display for SkillId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDef {
    pub id: SkillId,
    pub version: String,
    pub triggers: SkillTriggers,
    pub workflow: Vec<SkillStep>,
    pub prompt: String,
    pub knowledge_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTriggers {
    pub keywords: Vec<String>,
    pub regex: Vec<String>,
    pub domain: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillStep {
    pub id: String,
    pub description: String,
    pub tool: String,
    pub params: serde_json::Value,
    pub depends_on: Option<Vec<String>>,
    pub condition: Option<String>,
    pub fallback: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExperience {
    pub skill_id: SkillId,
    pub invoke_count: u64,
    pub success_rate: f64,
    pub avg_latency_ms: f64,
    pub last_invoked: Option<i64>,
    pub best_scenario: Option<String>,
    pub sm2: Sm2State,
    pub trend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sm2State {
    pub easiness: f64,
    pub interval_days: f64,
    pub repetition: u32,
}

impl Default for Sm2State {
    fn default() -> Self {
        Self { easiness: 2.5, interval_days: 1.0, repetition: 0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExecutionRecord {
    pub skill_id: SkillId,
    pub input: String,
    pub matched_triggers: Vec<String>,
    pub steps_executed: u32,
    pub total_steps: u32,
    pub total_latency_ms: u64,
    pub exit_code: u32,
    pub user_feedback: Option<String>,
    pub timestamp: i64,
}

// ─── Capability System ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityDeclaration {
    pub provider_id: String,
    pub capabilities: Vec<String>,
    pub constraints: Vec<String>,
    pub priority: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityRequest {
    pub kind: CapabilityKind,
    pub context: Option<CapabilityContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CapabilityKind {
    ToolExecution(ToolId),
    KnowledgeQuery { domain: String, query: String },
    LlmCompletion { model: String, capabilities: Vec<String> },
    ResourceAccess { resource: String, path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityContext {
    pub forced_provider: Option<String>,
    pub task_kind: Option<String>,
    pub session_id: Option<String>,
}

// ─── Plugin System ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    pub runtime: String,
    pub capabilities: Vec<String>,
    pub max_memory_mb: u32,
    pub max_instructions: u64,
    pub signature: Option<PluginSignature>,
    /// Tools exposed by this plugin (Phase 3 接入主 dispatch)
    /// 每个工具对应 wasm 模块导出函数 `{name}(params_ptr: i32, params_len: i32) -> i32`
    /// manifest.yaml 不带此字段时 default empty → plugin 加载但无暴露工具
    #[serde(default)]
    pub tools: Vec<PluginToolSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema 定义参数（同 ToolSchema.parameters）
    #[serde(default)]
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSignature {
    pub algorithm: String,
    pub value: String,
}

// ─── Core Loop Types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub tool_id: ToolId,
    pub success: bool,
    pub output: serde_json::Value,
    pub latency_ms: u64,
    /// Phase α-B: 失败原因显式分类（成功时为 None）
    ///
    /// ## 标准取值
    /// - `"NoExecutor"`：架构 gap，schema 注册了但 executor 缺位
    /// - `"Cooldown"`：工具处于冷却状态，N turn 后恢复
    /// - `"MCIPBlocked"`：MCIP 安全策略拒绝
    /// - `"Authorization"`：需要用户授权（NeedsConfirm 路径）
    /// - `"DestructiveOp"`：工具自身 schema.confirm_required=true 待用户确认
    /// - `"Timeout"`：执行超时
    /// - `"BusinessError"`：工具内部业务逻辑错误（默认兜底）
    ///
    /// ## 用途
    /// LLM 在下一轮看到 ToolMessage 时通过 failure_kind 字段精确判断：
    /// - Cooldown → 等几轮再试
    /// - NoExecutor → 直接放弃该工具，告诉用户「这是 bug」
    /// - MCIPBlocked → 显示拒绝原因，尝试 session.request_permission
    /// - 不再靠 error 文案匹配
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
    /// Phase α-H: 失败时建议的替代工具列表
    ///
    /// 例：filengine_bash_exec 失败可建议 ["filengine_fs_read", "filengine_fs_write"]
    /// 默认空——执行器主动填写时 LLM 看到「Try X instead」直接换工具不抖动。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub try_instead: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStats {
    pub turn_number: u32,
    pub tool_calls: u32,
    pub provider_id: String,
    pub model_id: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub total_tokens: u64,
    /// V30：思考 tokens（completion_tokens 子集；信息透明，不重复计费）
    /// 来源：TokenUsage.thinking_tokens（DeepSeek/OpenAI reasoning_tokens / Gemini thoughts）
    #[serde(default)]
    pub thinking_tokens: u64,
    pub latency_ms: u64,
    pub skills_matched: Vec<String>,
}

// ─── Security / Role System ────────────────────────────────────────────────

/// User role for MCIP access control.
///
/// Higher ordinal = more privileges.
/// - `Admin`: full access, no restrictions
/// - `Developer`: can use most tools; dangerous ones need confirmation
/// - `User`: restricted, all sensitive operations blocked or confirm-required
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[derive(Default)]
pub enum UserRole {
    User = 1,
    #[default]
    Developer = 2,
    Admin = 3,
}

impl UserRole {
    /// Return human-readable label
    pub fn label(&self) -> &'static str {
        match self {
            UserRole::User => "user",
            UserRole::Developer => "developer",
            UserRole::Admin => "admin",
        }
    }
}


impl std::fmt::Display for UserRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

// ─── MCP System ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerId(pub String);

impl From<&str> for ServerId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    pub server_id: ServerId,
    pub transport: String,
    pub address: String,
    pub tls: bool,
    pub request_signing: bool,
}