//! Agent 类型定义 — 外部 Agent 清单、能力、信任模型
//!
//! ## 设计意图
//! 外部 Agent 是自描述实体，通过 agent.yaml 声明身份、能力、工具、技能。
//! 安装后自动适配 Abacus 生态（工具注册、技能注册、Cluster 分配、Palace 记录）。
//!
//! ## 引用关系
//! - 消费方: abacus-core/src/agent/registry.rs (安装/卸载)
//! - 消费方: abacus-core/src/agent/executor.rs (执行)
//! - 消费方: abacus-orchestrator (Team/Meeting 集成)
//! - 消费方: abacus-cli/src/commands/agent.rs (CLI)

use serde::{Deserialize, Serialize};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// AgentManifest — 外部 Agent 清单（agent.yaml 的 Rust 映射）
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 外部 Agent 清单
///
/// ## 生命周期
/// - 创建: 从 agent.yaml 反序列化
/// - 消费: AgentRegistry::install() 时解析并注册
/// - 存储: ~/.abacus/agents/{id}/agent.yaml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    /// 唯一标识（如 "code-reviewer"）
    pub id: String,
    /// 显示名（如 "Code Reviewer"）
    pub name: String,
    /// 版本号（semver）
    pub version: String,
    /// 描述
    pub description: String,
    /// 传输配置
    pub transport: AgentTransport,
    /// 能力声明
    pub capabilities: AgentCapabilities,
    /// 暴露的工具列表
    #[serde(default)]
    pub tools: Vec<AgentToolSpec>,
    /// 暴露的技能列表
    #[serde(default)]
    pub skills: Vec<AgentSkillSpec>,
    /// 团队参与配置
    #[serde(default)]
    pub team: AgentTeamConfig,
    /// 信任级别
    #[serde(default)]
    pub trust: TrustLevel,
    /// 自适配配置
    #[serde(default)]
    pub adaptation: AdaptationConfig,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 传输层
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Agent 传输配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTransport {
    /// 传输类型: "mcp" | "http" | "wasm"
    #[serde(rename = "type")]
    pub transport_type: String,
    /// 端点地址
    /// - mcp: "npx -y @acme/agent-mcp"
    /// - http: "https://agent.example.com"
    /// - wasm: "./agent.wasm"
    pub endpoint: String,
    /// 认证配置（可选）
    #[serde(default)]
    pub auth: Option<AuthConfig>,
}

/// 认证配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// 认证类型: "bearer" | "api_key" | "basic"
    pub auth_type: String,
    /// 认证值（token / api_key / base64(user:pass)）
    pub value: String,
    /// 环境变量名（优先于 value）
    #[serde(default)]
    pub env_var: Option<String>,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 能力声明
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Agent 能力声明
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentCapabilities {
    /// 领域标签（如 ["code", "security", "performance"]）
    #[serde(default)]
    pub domains: Vec<String>,
    /// 动作标签（如 ["analyze", "review", "suggest"]）
    #[serde(default)]
    pub actions: Vec<String>,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 工具规格
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Agent 暴露的工具规格
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolSpec {
    /// 工具名（如 "review_code"）
    pub name: String,
    /// 工具描述
    pub description: String,
    /// JSON Schema 参数定义
    #[serde(default = "default_object_schema")]
    pub parameters: serde_json::Value,
}

fn default_object_schema() -> serde_json::Value {
    serde_json::json!({"type": "object", "properties": {}})
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 技能规格
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Agent 暴露的技能规格（工作流）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSkillSpec {
    /// 技能 ID（如 "full_review"）
    pub id: String,
    /// 触发条件
    #[serde(default)]
    pub trigger: SkillTrigger,
    /// 工作流步骤
    pub steps: Vec<AgentSkillStep>,
    /// 是否为复合技能（单工具调用，内部多步）
    #[serde(default)]
    pub compound: bool,
}

/// 技能触发条件
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillTrigger {
    /// 关键词匹配
    #[serde(default)]
    pub keywords: Vec<String>,
    /// 正则匹配
    #[serde(default)]
    pub regex: Vec<String>,
    /// 领域匹配
    #[serde(default)]
    pub domain: Vec<String>,
}

/// 技能工作流步骤
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSkillStep {
    /// 步骤 ID
    pub id: String,
    /// 步骤描述
    pub description: String,
    /// 调用的工具名
    pub tool: String,
    /// 工具参数（JSON）
    #[serde(default = "default_object_schema")]
    pub params: serde_json::Value,
    /// 依赖的前置步骤 ID
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// 失败时的回退步骤 ID
    #[serde(default)]
    pub fallback: Option<String>,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 团队配置
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Agent 团队参与配置
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentTeamConfig {
    /// 可担任的角色（如 ["Advisor", "Member"]）
    #[serde(default)]
    pub roles: Vec<String>,
    /// 专业领域（如 ["code quality", "security audit"]）
    #[serde(default)]
    pub expertise: Vec<String>,
    /// Meeting 路由标签（如 ["security", "code-review"]）
    #[serde(default)]
    pub meeting_tags: Vec<String>,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 信任模型
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Agent 信任级别
///
/// ## 安全策略
/// - Sandbox: 纯 WASM，无网络/文件，最大隔离
/// - Standard: MCP 协议，受限工具集，MCIP 策略适用（默认）
/// - Trusted: 已签名，完整工具集，MCIP 部分豁免
/// - Privileged: 系统级，完全访问，需用户显式授权
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TrustLevel {
    Sandbox,
    Standard,
    Trusted,
    Privileged,
}

impl Default for TrustLevel {
    fn default() -> Self {
        Self::Standard
    }
}

impl std::fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sandbox => write!(f, "sandbox"),
            Self::Standard => write!(f, "standard"),
            Self::Trusted => write!(f, "trusted"),
            Self::Privileged => write!(f, "privileged"),
        }
    }
}

impl TrustLevel {
    /// 从字符串解析
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "sandbox" => Some(Self::Sandbox),
            "standard" => Some(Self::Standard),
            "trusted" => Some(Self::Trusted),
            "privileged" => Some(Self::Privileged),
            _ => None,
        }
    }

    /// 是否允许网络访问
    pub fn allows_network(&self) -> bool {
        matches!(self, Self::Standard | Self::Trusted | Self::Privileged)
    }

    /// 是否允许文件系统访问
    pub fn allows_filesystem(&self) -> bool {
        matches!(self, Self::Trusted | Self::Privileged)
    }

    /// 是否需要用户确认
    pub fn requires_confirmation(&self) -> bool {
        matches!(self, Self::Standard | Self::Sandbox)
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 自适配配置
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Agent 自适配配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptationConfig {
    /// 安装后自动注册工具/技能
    #[serde(default = "default_true")]
    pub auto_register: bool,
    /// 记录执行历史到 BehaviorPalace
    #[serde(default = "default_true")]
    pub palace_enabled: bool,
    /// 自适配学习率
    #[serde(default = "default_learning_rate")]
    pub learning_rate: f64,
}

impl Default for AdaptationConfig {
    fn default() -> Self {
        Self {
            auto_register: true,
            palace_enabled: true,
            learning_rate: 0.1,
        }
    }
}

fn default_true() -> bool { true }
fn default_learning_rate() -> f64 { 0.1 }

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Agent 注册表条目
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 已安装 Agent 的持久化记录
///
/// 存储在 ~/.abacus/agents.toml 的 [[agents]] 数组中
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    /// Agent ID
    pub id: String,
    /// 安装来源（npm/git/local/mcp）
    pub source: String,
    /// 版本号
    pub version: String,
    /// 安装时间（ISO 8601）
    pub installed_at: String,
    /// 是否启用
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 信任级别覆盖（可选，优先于 manifest 中的值）
    #[serde(default)]
    pub trust_override: Option<TrustLevel>,
}

/// Agent 运行时状态
#[derive(Debug, Clone)]
pub struct AgentInstance {
    /// 清单
    pub manifest: AgentManifest,
    /// 连接状态
    pub connected: bool,
    /// 已注册的工具 ID 列表
    pub registered_tools: Vec<String>,
    /// 已注册的技能 ID 列表
    pub registered_skills: Vec<String>,
    /// 健康状态
    pub health: AgentHealth,
}

/// Agent 健康状态
#[derive(Debug, Clone)]
pub struct AgentHealth {
    /// 最后一次健康检查时间
    pub last_check: Option<std::time::Instant>,
    /// 是否可达
    pub reachable: bool,
    /// 平均延迟 (ms)
    pub avg_latency_ms: u64,
    /// 连续失败次数
    pub consecutive_failures: u32,
}

impl Default for AgentHealth {
    fn default() -> Self {
        Self {
            last_check: None,
            reachable: false,
            avg_latency_ms: 0,
            consecutive_failures: 0,
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Agent 调用请求/响应（跨 Agent 调用用）
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Agent 调用请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInvokeRequest {
    /// 要调用的技能/工具 ID
    pub target_id: String,
    /// 参数
    pub params: serde_json::Value,
    /// 调用上下文
    pub context: AgentInvokeContext,
}

/// Agent 调用上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInvokeContext {
    /// 发起方 session ID
    pub session_id: String,
    /// 当前 turn 编号
    pub turn_number: u32,
    /// 发起方最近使用的工具列表
    #[serde(default)]
    pub recent_tools: Vec<String>,
}

/// Agent 调用响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInvokeResponse {
    /// 是否成功
    pub success: bool,
    /// 输出内容（JSON 字符串）
    pub output: String,
    /// 延迟 (ms)
    pub latency_ms: u64,
    /// 错误信息（失败时）
    #[serde(default)]
    pub error: Option<String>,
}
