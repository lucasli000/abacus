//! L2 engine layer types — ToolRegistry, SkillEngine, CapabilityHub
//!
//! Pure data types without runtime dependencies. Runtime implementations
//! live in `abacus-core` crate.

use serde::{Deserialize, Serialize};

// ─── Tool System ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
    /// 加速 latency。fs_read / db_read_records 等读操作典型 idempotent；
    /// fs_write / bash_exec 等写操作必须 false。
    #[serde(default)]
    pub idempotent: bool,
    /// P0-C2: schema 是否跨 session 字节稳定（可参与 KV prefix cache）
    ///
    /// ## 何时标记为 true
    /// - schema 的 name / description / parameters 在运行时不会变化
    /// - 适用于内置稳定工具（db.* / kb.* / filengine.*）
    ///
    /// ## 影响路径
    /// build_tool_definitions_for() 将 schema_stable=true 的工具排在 tools 数组前部，
    /// 使 Anthropic/DeepSeek prefix cache 能稳定命中这段 schema bytes。
    /// 对于 schema_stable=false 的工具（lsp.* / mcp.* 等动态工具），排在后部，
    /// 变化只影响尾部不影响稳定前缀的缓存命中。
    ///
    /// ## 引用关系
    /// - 设置方：builtin 工具的 schemas() 函数（db/kb/filengine 标记为 true）
    /// - 消费方：CoreLoop::build_tool_definitions_for（排序优化，待实现）
    #[serde(default)]
    pub schema_stable: bool,
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

// ─── P1-C4: Skill 模板参数声明 ────────────────────────────────────────────

/// Skill 模板参数声明（P1-C4）
///
/// ## 设计
/// 允许 Skill YAML 声明模板参数，调用时用实际值替换 `{{param_name}}` 占位符。
/// 替换范围：prompt / SkillStep.params / SkillStep.description
///
/// ## YAML 示例
/// ```yaml
/// template_params:
///   - name: language
///     description: 目标语言（如 rust / python）
///     required: true
///   - name: max_issues
///     description: 最大问题数
///     required: false
///     default: "10"
/// ```
///
/// ## 占位符语法
/// `{{language}}` —— 必填（无默认值）
/// `{{max_issues}}` —— 可选（有默认值）
///
/// ## 引用关系
/// - 设置方：SkillDef.template_params
/// - 消费方：SkillEngine::render_with_params()
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillParamDecl {
    /// 参数名（对应 {{param_name}} 占位符）
    pub name: String,
    /// 参数描述（用于 LLM 调用建议 和 TUI 提示）
    pub description: String,
    /// 是否必填（默认 true）
    #[serde(default = "bool_true")]
    pub required: bool,
    /// 默认值（可选）
    #[serde(default)]
    pub default: Option<String>,
}

/// bool 默认值辅助函数（serde default）
fn bool_true() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDef {
    pub id: SkillId,
    pub version: String,
    pub triggers: SkillTriggers,
    pub workflow: Vec<SkillStep>,
    pub prompt: String,
    pub knowledge_refs: Vec<String>,
    /// P1-C4: Skill 模板参数声明列表
    ///
    /// 定义后，调用方可传入 `HashMap<String, String>` 进行占位符替换。
    /// 存量 YAML 无此字段时默认空 Vec，对应 Skill 直接执行不需要渲染。
    #[serde(default)]
    pub template_params: Vec<SkillParamDecl>,
    /// 行为宫殿标签：Skill 执行后向 BehaviorPalace 写入/查询时使用
    /// 引用: SkillExecutor 执行后调用 palace.record_interaction
    /// 生命周期: 随 SkillDef 静态存在
    #[serde(default)]
    pub palace_tags: Vec<String>,
    /// 复合执行模式：true = 执行器内部串联所有 step，只向 LLM 返回最终聚合结果（1条输出）
    /// false（默认）= 当前模式：每 step 是独立 tool call，LLM 看到所有中间结果
    ///
    /// ## 上下文管理
    /// compound=true 时，所有中间 step 结果不进入 session.messages，
    /// 大幅节省上下文 token（7 步 Skill 从 7 条 tool_output 降到 1 条）。
    ///
    /// 引用: SkillEngine::load_compound() + CompoundSkillExecutor
    /// 生命周期: 随 SkillDef 静态存在；序列化兼容——存量 JSON 无此字段时默认 false
    #[serde(default)]
    pub compound: bool,
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
    /// 例：bash_exec 失败可建议 ["fs_read", "fs_write"]
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
    /// 本轮结束后的上下文总占用（token）
    /// 来源：pipeline persist_and_build_result 从 context_manager.window 读取
    #[serde(default)]
    pub context_tokens: Option<u64>,
    /// 上下文窗口容量上限（token）
    #[serde(default)]
    pub context_max: Option<u64>,
}

// ─── Multi-Provider Configuration ──────────────────────────────────────────

/// 供应商配置条目（从 config.yaml `providers` 数组解析）
///
/// ## 引用关系
/// - 生产者: ConfigManager::parse_providers()
/// - 消费者: engine_init.rs 注册 ProviderGroup
///
/// ## 生命周期
/// - 创建: 启动时从配置文件解析
/// - 消费: engine_init 一次性读取注册后丢弃（配置值已持久化在 ProviderGroup 中）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub id: String,
    /// 供应商协议类型
    #[serde(rename = "type")]
    pub provider_type: ProviderType,
    /// API key — 支持 "env:VAR_NAME" 语法从环境变量读取
    #[serde(default)]
    pub api_key: Option<String>,
    /// API 端点 URL（可选，各 type 有默认值）
    #[serde(default)]
    pub base_url: Option<String>,
    /// 该 provider 下可用的模型列表
    /// 支持两种格式：
    /// - 简写: ["model-a", "model-b"]（纯字符串，用 ModelCatalog 默认参数）
    /// - 详写: [{name: "model-a", max_tokens: 8192, ...}]（对象，覆盖默认参数）
    #[serde(default, deserialize_with = "deserialize_model_entries")]
    pub models: Vec<ModelEntry>,
}

/// 单个模型配置（provider 内的 per-model 参数）
///
/// ## 引用关系
/// - 生产者: config.yaml providers[].models[] 解析
/// - 消费者: engine_init → ModelCatalog.register_override()
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// 模型 ID（必填）
    pub name: String,
    /// 上下文窗口大小（覆盖 ModelCatalog 默认值）
    #[serde(default)]
    pub context_window: Option<u64>,
    /// 单次最大输出 token
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// 生成温度 0.0-2.0
    #[serde(default)]
    pub temperature: Option<f64>,
    /// 思考模式: off / adaptive / low / medium / high / max
    #[serde(default)]
    pub thinking: Option<String>,
    /// Top-p 核采样
    #[serde(default)]
    pub top_p: Option<f64>,
    /// 是否支持图片输入
    #[serde(default)]
    pub supports_images: Option<bool>,
    /// 是否支持 tool_call
    #[serde(default)]
    pub supports_tools: Option<bool>,
}

/// 自定义反序列化：支持 models 字段的两种格式
/// - 字符串数组: ["model-a", "model-b"] → ModelEntry { name: "model-a", ... }
/// - 对象数组: [{name: "model-a", max_tokens: 8192}] → 直接解析
fn deserialize_model_entries<'de, D>(deserializer: D) -> Result<Vec<ModelEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct ModelEntriesVisitor;

    impl<'de> de::Visitor<'de> for ModelEntriesVisitor {
        type Value = Vec<ModelEntry>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a sequence of strings or model entry objects")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut entries = Vec::new();
            while let Some(item) = seq.next_element::<serde_json::Value>()? {
                match item {
                    serde_json::Value::String(name) => {
                        entries.push(ModelEntry {
                            name,
                            context_window: None,
                            max_tokens: None,
                            temperature: None,
                            thinking: None,
                            top_p: None,
                            supports_images: None,
                            supports_tools: None,
                        });
                    }
                    serde_json::Value::Object(_) => {
                        let entry: ModelEntry = serde_json::from_value(item)
                            .map_err(de::Error::custom)?;
                        entries.push(entry);
                    }
                    _ => return Err(de::Error::custom("model entry must be string or object")),
                }
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_seq(ModelEntriesVisitor)
}

/// 供应商协议类型
///
/// 决定使用哪个 Provider 实现（AnthropicProvider / OpenAICompatibleProvider / DeepSeekProvider）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderType {
    Anthropic,
    OpenaiCompatible,
    Deepseek,
    Gemini,
}

// ─── Security / Role System ────────────────────────────────────────────────

/// bash 执行策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BashPolicyLevel {
    ReadOnly,
    #[default]
    DevTools,
    Full,
}

/// 搜索 provider 抽象
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SearchProvider {
    BraveApi { api_key: String },
    SearxNg { base_url: String },
    DuckDuckGo,
}

impl Default for SearchProvider {
    fn default() -> Self { Self::DuckDuckGo }
}

/// Role 能力声明——将限制从工具内移到 Role 层
///
/// 引用: ExecutionContext.role_caps; CoreLoop::new() 从 config 构建
/// 生命周期: CoreLoop 内存持有; Arc 照顾 ExecutionContext 剪取
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleCapabilities {
    /// 文件系统可访问根目录（替代 filengine.allowed_roots()）
    pub fs_roots: Vec<String>,
    pub bash_policy: BashPolicyLevel,
    /// None=不限，Some(vec![])=禁止所有
    pub web_domains: Option<Vec<String>>,
    pub tool_budget_per_turn: u32,
    pub search_provider: SearchProvider,
}

impl Default for RoleCapabilities {
    fn default() -> Self {
        // fs_roots 默认用 $HOME，与 filengine::allowed_roots() 保持一致
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| "/tmp".into());
        Self {
            fs_roots: vec![home],
            bash_policy: BashPolicyLevel::DevTools,
            web_domains: None,
            tool_budget_per_turn: 20,
            search_provider: SearchProvider::DuckDuckGo,
        }
    }
}

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