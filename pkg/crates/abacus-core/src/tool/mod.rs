//! ToolRegistry — 工具注册中心与统一执行接口
//!
//! ## Dependencies (external crates)
//! - `tokio::sync::RwLock`: concurrent read/write access to tool registry
//! - `serde_json::Value`: tool parameter and result serialization
//!
//! ## Dependencies (internal)
//! - `abacus_types::{ToolHandle, ToolId, ToolOutput, ToolState, VisibilityTier}`: tool data types
//! - `abacus_types::ToolEffectiveness`: effectiveness tracking for visibility tiers
//! - `crate::tool::builtin`: built-in tool implementations (filengine, code_exec)
//! - `crate::tool::effectiveness`: EffectivenessTracker for scoring and tier assignment
//!
//! ## References (callers)
//! - `crate::core::mod.rs::CoreLoop::new()` → creates ToolRegistry, registers all builtins
//! - `crate::core::mod.rs::CoreLoop::process_turn()` → calls `registry.execute()` per tool call
//! - `crate::core::mod.rs::CoreLoop::build_tool_definitions()` → calls `registry.all_tools()`
//! - `crate::skill::mod.rs::SkillEngine::load()` → registers skill workflow steps as tools
//! - `crate::mcp.rs::McpClient::discover_tools()` → registers MCP-discovered tools
//!
//! ## Referenced by
//! - `EffectivenessTracker` (effectiveness.rs): reads/writes tool effectiveness data
//! - `CapabilityHub` (capability/mod.rs): resolves tool execution requests to providers
//!
//! ## Design Notes
//! - `execute()` returns `ToolOutput { success: false }` on executor missing or failure,
//!   NOT `Err`, to avoid interrupting multi-turn conversation loops.
//! - `LazyToolLoader` trait enables on-demand tool preparation (e.g., checking Python is installed).

use std::collections::HashMap;
use std::sync::Arc;

use abacus_types::{
    ToolEffectiveness, ToolHandle, ToolId, ToolOutput, ToolState, VisibilityTier,
};
use serde_json::Value;
use tokio::sync::RwLock;

pub mod builtin;
pub mod cluster;
pub mod effectiveness;
pub mod schema_lint;
pub mod subsystem_policy;

/// Lazy loader trait for on-demand tool preparation
#[async_trait::async_trait]
pub trait LazyToolLoader: Send + Sync {
    async fn prepare(&self, tool_id: &ToolId) -> LazyLoadResult;
}

pub enum LazyLoadResult {
    Ready,
    Blocked { reason: String },
    NotFound,
}

/// Per-request execution context — 传递给每次工具调用
///
/// ## 设计
/// 替代原先"全局共享 executor 持有 session Arc"的架构缺陷。
/// ExecutionContext 在 pipeline Phase 4 每次工具调用前构建，
/// 携带当前 request 专属的 per-session 状态（filengine cwd/modified 等）。
///
/// ## 生命周期
/// - 创建：`TurnPipeline::execute_loop()` 进入工具分发时
/// - 消费：`ToolRegistry::execute()` → `ToolExecutor::execute()`
/// - 销毁：工具返回后随 `&` 引用一同 drop（不 clone，不缓存）
///
/// ## 扩展
/// 未来添加 per-session 状态只需在此结构体中新增字段，
/// 无状态工具继续忽略 `_ctx`，影响面为零。
#[derive(Clone)]
pub struct ExecutionContext {
    /// 当前 session 标识（用于日志和调试）
    pub session_id: String,
    /// Per-session filengine 状态（cwd / modified 文件集 / undo logger）
    ///
    /// 只有 `FilengineToolExecutor` 读取此字段；
    /// 其他所有 executor 对此字段完全忽略。
    pub filengine: Arc<tokio::sync::RwLock<crate::tool::builtin::filengine::FilengineSession>>,
    /// 当前 turn 编号（Phase 2 undo：filengine 写工具 commit 时透传到 LogEntry.turn）
    ///
    /// 由 `TurnPipeline` 构建 ExecutionContext 时从 `TurnContext.turn_number` 注入。
    /// 默认 0（noop / 无 turn 上下文场景）。
    pub turn_number: u32,
    /// Bash 默认超时（秒）— 从 policy.toml 注入
    pub bash_default_timeout: u64,
    /// Bash 最大超时（秒）— 从 policy.toml 注入
    pub bash_max_timeout: u64,
    /// 通用工具执行超时（秒）— 从 policy.toml 注入
    ///
    /// 引用关系：由 pipeline 从 PolicyConfig.thresholds.tool_default_timeout 注入
    /// 消费方：ToolRegistry::execute() 用作非 bash 工具的安全网超时
    /// Bash 工具有独立内部超时机制，此字段对 bash 是冗余安全网（仍生效但较宽松）
    pub tool_default_timeout: u64,
}

impl ExecutionContext {
    /// 测试 / 批处理场景：创建无状态默认 context
    ///
    /// 使用全新的 `FilengineSession`（cwd = 进程工作目录），turn_number=0。
    /// 适用于：单元测试、`auto::Pipeline`、`PlanExecutor::StepKind::ToolCall` 等无用户 session 的场景。
    pub fn noop(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            filengine: Arc::new(tokio::sync::RwLock::new(
                crate::tool::builtin::filengine::FilengineSession::new()
            )),
            turn_number: 0,
            bash_default_timeout: 30,
            bash_max_timeout: 120,
            tool_default_timeout: 60,
        }
    }
}

/// Unified executor for all tools (built-in, MCP, plugin, skill).
///
/// ## ExecutionContext 参数
/// 携带 per-request session 状态（当前 cwd、已修改文件集等）。
/// 无状态工具直接以 `_ctx: &ExecutionContext` 命名参数，编译器静默忽略。
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, tool_id: &ToolId, params: Value, ctx: &ExecutionContext) -> abacus_types::Result<Value>;
}

pub struct ToolRegistry {
    tools: RwLock<HashMap<ToolId, ToolHandle>>,
    executors: RwLock<HashMap<ToolId, Arc<dyn ToolExecutor>>>,
    lazy_loaders: Vec<Box<dyn LazyToolLoader>>,
    #[allow(dead_code)]
    init_order: Vec<ToolId>,
    /// Cached snapshot of all tools (invalidated on register/remove).
    tools_cache: RwLock<Option<Vec<ToolHandle>>>,
    /// Phase 1：注册时静态 Lint 规则集
    ///
    /// 引用关系：register() 调用；audit 路径读取 lint_issues。
    /// 默认规则集在 ToolRegistry::new 注入；用户可通过 set_lint_rules 覆盖。
    /// 当前（Phase 1）仅写日志 + 累积——不 panic。Phase 3 接通 panic。
    pub(crate) lint_rules: RwLock<schema_lint::LintRuleSet>,
    /// 累积的 lint issues —— 给 audit 报告用
    pub(crate) lint_issues: RwLock<Vec<schema_lint::LintIssue>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            executors: RwLock::new(HashMap::new()),
            lazy_loaders: Vec::new(),
            init_order: Vec::new(),
            tools_cache: RwLock::new(None),
            lint_rules: RwLock::new(schema_lint::LintRuleSet::default_rules()),
            lint_issues: RwLock::new(Vec::new()),
        }
    }

    /// Phase 1：替换 lint 规则集（如加载 YAML config 后注入 allowed list）
    pub async fn set_lint_rules(&self, rules: schema_lint::LintRuleSet) {
        *self.lint_rules.write().await = rules;
    }

    /// Phase 1：审计接口——返回累积的 lint issues 副本
    ///
    /// 引用关系：被 Layer 5 audit_optimizations 调用；不修改累积状态。
    pub async fn lint_audit(&self) -> Vec<schema_lint::LintIssue> {
        self.lint_issues.read().await.clone()
    }

    /// Register a tool at any point in the lifecycle
    pub async fn register(&self, handle: ToolHandle) {
        // Phase 1：注册前静态 Lint 检查
        // 副作用：写日志 + 累积到 lint_issues；Phase 3 加 panic 路径
        let issues = {
            let rules = self.lint_rules.read().await;
            rules.lint(&handle.schema, &handle.id)
        };
        for issue in &issues {
            schema_lint::handle_issue(issue);
        }
        if !issues.is_empty() {
            self.lint_issues.write().await.extend(issues);
        }

        let mut tools = self.tools.write().await;
        tools.insert(handle.id.clone(), handle);
        // Invalidate cache
        *self.tools_cache.write().await = None;
    }

    /// Register multiple tools (batch, during init)
    pub async fn register_batch(&self, handles: Vec<ToolHandle>) {
        let mut tools = self.tools.write().await;
        for h in handles {
            let id = h.id.clone();
            tools.insert(id, h);
        }
    }

    /// Look up a tool by id
    pub async fn get(&self, id: &ToolId) -> Option<ToolHandle> {
        self.tools.read().await.get(id).cloned()
    }

    /// Prepare a lazy-loaded tool (check environment)
    pub async fn prepare(&self, id: &ToolId) -> LazyLoadResult {
        let handle = {
            let tools = self.tools.read().await;
            tools.get(id).cloned()
        };
        let handle = match handle {
            Some(h) => h,
            None => return LazyLoadResult::NotFound,
        };
        if handle.state != ToolState::Registered {
            return LazyLoadResult::Ready;
        }
        for loader in &self.lazy_loaders {
            let result = loader.prepare(id).await;
            match &result {
                LazyLoadResult::Ready => {
                    let mut tools = self.tools.write().await;
                    if let Some(h) = tools.get_mut(id) {
                        h.state = ToolState::Loaded;
                    }
                    return LazyLoadResult::Ready;
                }
                LazyLoadResult::Blocked { .. } => {
                    let mut tools = self.tools.write().await;
                    if let Some(h) = tools.get_mut(id) {
                        h.state = ToolState::Disabled;
                        h.effectiveness = ToolEffectiveness {
                            blocked_by_env: true,
                            ..Default::default()
                        };
                    }
                    return result;
                }
                LazyLoadResult::NotFound => continue,
            }
        }
        LazyLoadResult::NotFound
    }

    /// Mark a tool as active after it has been called
    pub async fn mark_active(&self, id: &ToolId) {
        let mut tools = self.tools.write().await;
        if let Some(h) = tools.get_mut(id) {
            h.state = ToolState::Active;
        }
    }

    pub async fn mark_cooling(&self, id: &ToolId, cooldown: u32) {
        let mut tools = self.tools.write().await;
        if let Some(h) = tools.get_mut(id) {
            h.state = ToolState::Cooling;
            h.effectiveness.cooldown_remaining = cooldown;
        }
    }

    pub async fn mark_disabled(&self, id: &ToolId) {
        let mut tools = self.tools.write().await;
        if let Some(h) = tools.get_mut(id) {
            h.state = ToolState::Disabled;
        }
    }

    /// Decrement cooldown counters for all cooling tools by one turn.
    /// When cooldown reaches 0, the tool transitions back to Loaded state.
    /// Called once per turn at the end of process_turn.
    pub async fn tick_cooldowns(&self) {
        let mut tools = self.tools.write().await;
        for h in tools.values_mut() {
            if h.state == ToolState::Cooling && h.effectiveness.cooldown_remaining > 0 {
                h.effectiveness.cooldown_remaining -= 1;
                if h.effectiveness.cooldown_remaining == 0 {
                    h.state = ToolState::Loaded;
                }
            }
        }
    }

    /// Register a tool executor alongside its handle
    pub async fn register_executor(&self, id: ToolId, executor: Arc<dyn ToolExecutor>) {
        self.executors.write().await.insert(id, executor);
    }

    /// Execute a tool by id with the given parameters.
    /// Returns ToolOutput with success=false instead of propagating errors,
    /// so that a single tool failure does not interrupt the multi-turn conversation.
    ///
    /// ## ctx 参数
    /// 携带 per-request session 状态（filengine session、session_id 等）。
    /// 由调用方（pipeline / auto::pipeline / plan executor）在调用前构建并传入。
    /// 无 session 场景使用 `ExecutionContext::noop(id)` 占位。
    pub async fn execute(&self, tool_id: &ToolId, params: Value, ctx: &ExecutionContext) -> abacus_types::Result<ToolOutput> {
        // Check if tool is in cooling state — reject execution per design (§5.5)
        {
            let tools = self.tools.read().await;
            if let Some(h) = tools.get(tool_id) {
                if h.state == ToolState::Cooling {
                    return Ok(ToolOutput {
                        tool_id: tool_id.clone(),
                        success: false,
                        output: serde_json::json!({"error": format!("tool in cooldown: {tool_id}, {} turns remaining", h.effectiveness.cooldown_remaining)}),
                        latency_ms: 0,
                        failure_kind: Some("Cooldown".into()),
                        try_instead: Vec::new(),
                    });
                }
            }
        }

        let executor = {
            let executors = self.executors.read().await;
            executors.get(tool_id).cloned()
        };
        match executor {
            Some(exe) => {
                let start = std::time::Instant::now();
                let timeout_secs = ctx.tool_default_timeout;

                // Panic isolation + timeout safety net
                //
                // 引用关系：
                // - tokio::spawn 隔离 executor panic（不崩溃调用方 task）
                // - tokio::time::timeout 防止 executor 无限挂起
                //
                // 生命周期：
                // - spawned task 在 timeout 或完成后销毁
                // - timeout 到期时 spawned task 被 abort（资源释放）
                let tid = tool_id.clone();
                let ctx_clone = ctx.clone();
                let handle = tokio::spawn(async move {
                    exe.execute(&tid, params, &ctx_clone).await
                });

                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(timeout_secs),
                    handle,
                ).await;

                let latency = start.elapsed().as_millis() as u64;

                match result {
                    // 正常完成 + executor 返回 Ok
                    Ok(Ok(Ok(output))) => Ok(ToolOutput {
                        tool_id: tool_id.clone(),
                        success: true,
                        output,
                        latency_ms: latency,
                        failure_kind: None,
                        try_instead: Vec::new(),
                    }),
                    // 正常完成 + executor 返回 Err（业务错误）
                    Ok(Ok(Err(e))) => Ok(ToolOutput {
                        tool_id: tool_id.clone(),
                        success: false,
                        output: serde_json::json!({"error": e.to_string()}),
                        latency_ms: latency,
                        failure_kind: Some("BusinessError".into()),
                        try_instead: Vec::new(),
                    }),
                    // Executor panic（JoinError）— 隔离成功，不崩溃调用方
                    Ok(Err(join_err)) => {
                        tracing::error!("tool executor panicked: {tool_id} — {join_err}");
                        Ok(ToolOutput {
                            tool_id: tool_id.clone(),
                            success: false,
                            output: serde_json::json!({"error": format!("executor panicked: {tool_id}")}),
                            latency_ms: latency,
                            failure_kind: Some("Panic".into()),
                            try_instead: Vec::new(),
                        })
                    }
                    // 超时 — abort spawned task，释放资源
                    Err(_elapsed) => {
                        tracing::warn!("tool execution timeout: {tool_id} after {timeout_secs}s");
                        Ok(ToolOutput {
                            tool_id: tool_id.clone(),
                            success: false,
                            output: serde_json::json!({"error": format!("timeout after {timeout_secs}s")}),
                            latency_ms: latency,
                            failure_kind: Some("Timeout".into()),
                            try_instead: Vec::new(),
                        })
                    }
                }
            }
            None => Ok(ToolOutput {
                tool_id: tool_id.clone(),
                success: false,
                output: serde_json::json!({"error": format!("no executor for tool: {tool_id}")}),
                latency_ms: 0,
                failure_kind: Some("NoExecutor".into()),
                try_instead: Vec::new(),
            }),
        }
    }

    /// Remove an executor (e.g. when a tool is disabled)
    pub async fn remove_executor(&self, id: &ToolId) {
        self.executors.write().await.remove(id);
    }

    pub async fn list_visible(&self, tier: VisibilityTier) -> Vec<ToolHandle> {
        let tools = self.tools.read().await;
        tools
            .values()
            .filter(|t| {
                t.state == ToolState::Loaded
                    || t.state == ToolState::Active
            })
            .filter(|t| tier_visible(&t.effectiveness.tier, &tier))
            .cloned()
            .collect()
    }

    pub async fn all_tools(&self) -> Vec<ToolHandle> {
        // Return cached snapshot if available
        {
            let cache = self.tools_cache.read().await;
            if let Some(ref cached) = *cache {
                return cached.clone();
            }
        }
        // Rebuild cache
        let tools: Vec<ToolHandle> = self.tools.read().await.values().cloned().collect();
        *self.tools_cache.write().await = Some(tools.clone());
        tools
    }

    pub fn add_lazy_loader(&mut self, loader: Box<dyn LazyToolLoader>) {
        self.lazy_loaders.push(loader);
    }

    /// 返回所有已注册工具名列表（用于 tool-in-text 检测）
    pub async fn tool_names(&self) -> Vec<String> {
        self.tools.read().await.keys().map(|k| k.0.clone()).collect()
    }
}

/// True if `tool_tier` meets the minimum `threshold` tier
fn tier_visible(tool_tier: &VisibilityTier, threshold: &VisibilityTier) -> bool {
    fn rank(t: &VisibilityTier) -> u8 {
        match t {
            VisibilityTier::S => 5,
            VisibilityTier::A => 4,
            VisibilityTier::B => 3,
            VisibilityTier::C => 2,
            VisibilityTier::D => 1,
        }
    }
    rank(tool_tier) >= rank(threshold)
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::{ToolProvider, ToolSchema};

    #[tokio::test]
    async fn test_register_and_get() {
        let reg = ToolRegistry::new();
        let tool = ToolHandle {
            id: ToolId("test_tool".into()),
            schema: ToolSchema {
                name: "test_tool".into(),
                description: "A test tool".into(),
                parameters: serde_json::json!({}),
                returns: None,
                security: None,
                cost: None,
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: false,
            },
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        };
        reg.register(tool).await;
        let result = reg.get(&ToolId("test_tool".into())).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().id.0, "test_tool");
    }

    #[tokio::test]
    async fn test_prepare_not_found() {
        let reg = ToolRegistry::new();
        let result = reg.prepare(&ToolId("nonexistent".into())).await;
        assert!(matches!(result, LazyLoadResult::NotFound));
    }
}