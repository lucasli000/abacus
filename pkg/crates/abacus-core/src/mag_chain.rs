//! Mag Chain — 中间件链
//!
//! ## 依赖
//! - `abacus_types::ToolOutput`: 工具输出
//! - `abacus_types::KernelError`: 错误类型
//!
//! ## 引用关系
//! - 被 `CoreLoop::process_turn` 在工具执行前后调用
//! - 包装 `ToolRegistry::execute` 的结果
//!
//! ## 中间件列表
//! 1. CircuitBreaker — 熔断器 (连续失败 N 次后熔断)
//! 2. RateLimiter — 限流器 (滑动窗口限频)
//! 3. AuditLogger — 审计日志
//! 4. PiiRedactor — 敏感信息脱敏
//! 5. RetryMiddleware — 重试中间件

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use abacus_types::{ToolId, ToolOutput, KernelError};

/// 中间件 trait
#[async_trait::async_trait]
pub trait Middleware: Send + Sync {
    /// 工具执行前钩子
    async fn before_execute(&self, tool_id: &ToolId, params: &serde_json::Value) -> Result<(), KernelError>;
    /// 工具执行后钩子 (mutable — allows redaction, transformation)
    async fn after_execute(&self, tool_id: &ToolId, output: &mut ToolOutput) -> Result<(), KernelError>;
    /// 中间件名称
    fn name(&self) -> &str;
}

/// Mag Chain — 中间件链 (config-driven ordering via priority)
pub struct MagChain {
    middlewares: Vec<(u32, Arc<dyn Middleware>)>,
}

impl MagChain {
    pub fn new() -> Self {
        Self { middlewares: Vec::new() }
    }

    /// Add middleware with default priority (100).
    pub fn add(&mut self, middleware: Arc<dyn Middleware>) {
        self.add_with_priority(100, middleware);
    }

    /// Add middleware with explicit priority (lower = earlier execution).
    pub fn add_with_priority(&mut self, priority: u32, middleware: Arc<dyn Middleware>) {
        self.middlewares.push((priority, middleware));
        self.middlewares.sort_by_key(|(p, _)| *p);
    }

    pub async fn before(&self, tool_id: &ToolId, params: &serde_json::Value) -> Result<(), KernelError> {
        for (_, mw) in &self.middlewares {
            mw.before_execute(tool_id, params).await?;
        }
        Ok(())
    }

    pub async fn after(&self, tool_id: &ToolId, output: &mut ToolOutput) -> Result<(), KernelError> {
        for (_, mw) in &self.middlewares {
            mw.after_execute(tool_id, output).await?;
        }
        Ok(())
    }

    pub fn len(&self) -> usize { self.middlewares.len() }
    pub fn is_empty(&self) -> bool { self.middlewares.is_empty() }
}

impl Default for MagChain {
    fn default() -> Self { Self::new() }
}

// ─── Circuit Breaker ───────────────────────────────────────────────────

/// 熔断器中间件
pub struct CircuitBreaker {
    state: RwLock<HashMap<ToolId, CircuitState>>,
    failure_threshold: u32,
    recovery_timeout: Duration,
}

struct CircuitState {
    failures: u32,
    last_failure: Option<Instant>,
    open: bool,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, recovery_timeout: Duration) -> Self {
        Self {
            state: RwLock::new(HashMap::new()),
            failure_threshold,
            recovery_timeout,
        }
    }
}

#[async_trait::async_trait]
impl Middleware for CircuitBreaker {
    async fn before_execute(&self, tool_id: &ToolId, _params: &serde_json::Value) -> Result<(), KernelError> {
        let mut state = self.state.write().await;
        let cs = state.entry(tool_id.clone()).or_insert(CircuitState {
            failures: 0,
            last_failure: None,
            open: false,
        });

        if cs.open {
            if let Some(last) = cs.last_failure {
                if last.elapsed() > self.recovery_timeout {
                    cs.open = false;
                    cs.failures = 0;
                    return Ok(());
                }
            }
            return Err(KernelError::Other(format!("circuit open for tool: {tool_id}")));
        }
        Ok(())
    }

    async fn after_execute(&self, tool_id: &ToolId, output: &mut ToolOutput) -> Result<(), KernelError> {
        let mut state = self.state.write().await;
        let cs = state.entry(tool_id.clone()).or_insert(CircuitState {
            failures: 0,
            last_failure: None,
            open: false,
        });

        if !output.success {
            cs.failures += 1;
            cs.last_failure = Some(Instant::now());
            if cs.failures >= self.failure_threshold {
                cs.open = true;
            }
        } else {
            cs.failures = 0;
            cs.open = false;
        }
        Ok(())
    }

    fn name(&self) -> &str { "circuit_breaker" }
}

// ─── Rate Limiter ──────────────────────────────────────────────────────

/// Phase Stability-1：基于 governor (GCRA lock-free) 的限流器
///
/// ## 替换前后对比
/// - 旧：每工具一个 `Vec<Instant>` 滑动窗口，O(n) 扫描+清理；写锁串行化整个 HashMap
/// - 新：每工具一个 `governor::DefaultDirectRateLimiter`，**O(1) lock-free GCRA**
///   决策；HashMap 仍需写锁但只在首次见到 tool_id 时插入，后续读路径无写
///
/// ## 语义保持
/// 公共接口签名不变（`new(max_calls, window)`、`before_execute`、`after_execute`、`name()`），
/// 替换对调用方透明。
///
/// ## 速率精度
/// `max_calls` per `window` 转换为"每秒允许速率 + 突发桶"：
///   - rate = max_calls / window.as_secs_f64()
///   - burst = max_calls （允许突发后立即耗尽窗口配额）
/// 若 window 极短（< 1ms）或 max_calls=0，回退到允许所有请求（保护性）
pub struct RateLimiter {
    limiters: tokio::sync::RwLock<
        HashMap<
            ToolId,
            std::sync::Arc<
                governor::RateLimiter<
                    governor::state::NotKeyed,
                    governor::state::InMemoryState,
                    governor::clock::DefaultClock,
                    governor::middleware::NoOpMiddleware,
                >,
            >,
        >,
    >,
    quota: governor::Quota,
}

impl RateLimiter {
    pub fn new(max_calls: u32, window: Duration) -> Self {
        // 把 max_calls/window 翻成 governor 的 Quota
        // Quota::with_period 给"每多久放 1 个 token"；burst_size 给突发桶大小。
        let period = if max_calls == 0 || window.is_zero() {
            // 兜底：超快放 1 token / 1ns，等于不限流（保护性）
            Duration::from_nanos(1)
        } else {
            window / max_calls
        };
        let burst = std::num::NonZeroU32::new(max_calls.max(1)).expect("max_calls.max(1) > 0");
        let quota = governor::Quota::with_period(period)
            .unwrap_or_else(|| governor::Quota::per_second(burst))
            .allow_burst(burst);
        Self {
            limiters: tokio::sync::RwLock::new(HashMap::new()),
            quota,
        }
    }

    /// 取或建某 tool_id 的 governor 限流器。
    /// 单写锁 fast-path：先 read 命中即返回，miss 升级 write 插入新 limiter。
    async fn limiter_for(
        &self,
        tool_id: &ToolId,
    ) -> std::sync::Arc<
        governor::RateLimiter<
            governor::state::NotKeyed,
            governor::state::InMemoryState,
            governor::clock::DefaultClock,
            governor::middleware::NoOpMiddleware,
        >,
    > {
        if let Some(l) = self.limiters.read().await.get(tool_id) {
            return l.clone();
        }
        let mut w = self.limiters.write().await;
        // double-check 防 TOCTOU
        if let Some(l) = w.get(tool_id) {
            return l.clone();
        }
        let new = std::sync::Arc::new(governor::RateLimiter::direct(self.quota));
        w.insert(tool_id.clone(), new.clone());
        new
    }
}

#[async_trait::async_trait]
impl Middleware for RateLimiter {
    async fn before_execute(&self, tool_id: &ToolId, _params: &serde_json::Value) -> Result<(), KernelError> {
        let limiter = self.limiter_for(tool_id).await;
        // governor `check()` 是 lock-free O(1)：返回 Ok(()) 或 Err(NotUntil)
        match limiter.check() {
            Ok(_) => Ok(()),
            Err(_) => Err(KernelError::Other(format!("rate limit exceeded for tool: {tool_id}"))),
        }
    }

    async fn after_execute(&self, _tool_id: &ToolId, _output: &mut ToolOutput) -> Result<(), KernelError> {
        Ok(())
    }

    fn name(&self) -> &str { "rate_limiter" }
}

// ─── Audit Logger ──────────────────────────────────────────────────────

/// 审计日志中间件
pub struct AuditLogger {
    log: RwLock<Vec<AuditEntry>>,
    max_entries: usize,
}

#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub tool_id: String,
    pub success: bool,
    pub latency_ms: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl AuditLogger {
    pub fn new(max_entries: usize) -> Self {
        Self {
            log: RwLock::new(Vec::new()),
            max_entries,
        }
    }

    pub async fn entries(&self) -> Vec<AuditEntry> {
        self.log.read().await.clone()
    }
}

#[async_trait::async_trait]
impl Middleware for AuditLogger {
    async fn before_execute(&self, _tool_id: &ToolId, _params: &serde_json::Value) -> Result<(), KernelError> {
        Ok(())
    }

    async fn after_execute(&self, tool_id: &ToolId, output: &mut ToolOutput) -> Result<(), KernelError> {
        let mut log = self.log.write().await;
        log.push(AuditEntry {
            tool_id: tool_id.0.clone(),
            success: output.success,
            latency_ms: output.latency_ms,
            timestamp: chrono::Utc::now(),
        });
        if log.len() > self.max_entries {
            log.remove(0);
        }
        Ok(())
    }

    fn name(&self) -> &str { "audit_logger" }
}

// ─── PII Redactor ──────────────────────────────────────────────────────

/// 敏感信息脱敏中间件
pub struct PiiRedactor {
    patterns: Vec<regex::Regex>,
    replacement: String,
}

impl PiiRedactor {
    pub fn new() -> Self {
        let patterns = vec![
            regex::Regex::new(r"\b\d{4}[- ]?\d{4}[- ]?\d{4}[- ]?\d{4}\b").unwrap(), // credit card
            regex::Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Z|a-z]{2,}\b").unwrap(), // email
            regex::Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap(), // SSN
        ];
        Self {
            patterns,
            replacement: "[REDACTED]".into(),
        }
    }

    /// cross-session 段F：复用脱敏逻辑给非工具场景（如 history.jsonl 写入）
    ///
    /// ## 引用关系
    /// - 上游：`GlobalHistoryHook::on_event` 在写入 prompt 前调
    /// - 内部：复用 self.patterns（Vec<Regex>）顺序应用
    ///
    /// ## 行为
    /// 输入是 user prompt 字符串，输出是已脱敏副本。原字符串不修改。
    /// 多个 pattern 顺序应用——同一段 PII 即使匹配多次也只替换一次（regex replace_all 自然语义）。
    pub fn redact_string(&self, input: &str) -> String {
        let mut result = input.to_string();
        for pattern in &self.patterns {
            result = pattern.replace_all(&result, self.replacement.as_str()).to_string();
        }
        result
    }
}

impl Default for PiiRedactor {
    fn default() -> Self { Self::new() }
}

#[async_trait::async_trait]
impl Middleware for PiiRedactor {
    async fn before_execute(&self, _tool_id: &ToolId, _params: &serde_json::Value) -> Result<(), KernelError> {
        Ok(())
    }

    async fn after_execute(&self, _tool_id: &ToolId, output: &mut ToolOutput) -> Result<(), KernelError> {
        // Recursively redact PII from all string values in the output
        redact_value(&mut output.output, &self.patterns, &self.replacement);
        Ok(())
    }

    fn name(&self) -> &str { "pii_redactor" }
}

/// Recursively redact PII from a JSON value
fn redact_value(v: &mut serde_json::Value, patterns: &[regex::Regex], replacement: &str) {
    match v {
        serde_json::Value::String(s) => {
            for pattern in patterns {
                *s = pattern.replace_all(s, replacement).to_string();
            }
        }
        serde_json::Value::Object(map) => {
            for (_, val) in map.iter_mut() {
                redact_value(val, patterns, replacement);
            }
        }
        serde_json::Value::Array(arr) => {
            for val in arr.iter_mut() {
                redact_value(val, patterns, replacement);
            }
        }
        _ => {}
    }
}

// ─── Persistent Audit Logger ─────────────────────────────────────────────

/// Audit logger backed by SQLite for cross-session persistence.
pub struct PersistentAuditLogger {
    db: tokio::sync::Mutex<rusqlite::Connection>,
    max_entries: usize,
}

impl PersistentAuditLogger {
    /// Open or create the audit database at `path`.
    pub fn new(path: PathBuf, max_entries: usize) -> Result<Self, KernelError> {
        let conn = rusqlite::Connection::open(&path)
            .map_err(|e| KernelError::Other(format!("failed to open audit db: {e}")))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS audit_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                tool_id TEXT NOT NULL,
                success INTEGER NOT NULL,
                latency_ms INTEGER NOT NULL,
                timestamp TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_audit_tool ON audit_log(tool_id);
            CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_log(timestamp);"
        ).map_err(|e| KernelError::Other(format!("failed to create audit table: {e}")))?;
        Ok(Self { db: tokio::sync::Mutex::new(conn), max_entries })
    }

    /// Load recent entries from the database.
    pub async fn entries(&self) -> Vec<AuditEntry> {
        let db = self.db.lock().await;
        let mut stmt = match db.prepare(
            "SELECT tool_id, success, latency_ms, timestamp FROM audit_log ORDER BY id DESC LIMIT ?"
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([self.max_entries as i64], |row| {
            let tool_id: String = row.get(0)?;
            let success: bool = row.get::<_, i32>(1)? != 0;
            let latency_ms: u64 = row.get(2)?;
            let ts_str: String = row.get(3)?;
            let timestamp: chrono::DateTime<chrono::Utc> = ts_str.parse().unwrap_or_else(|_| chrono::Utc::now());
            Ok(AuditEntry { tool_id, success, latency_ms, timestamp })
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut entries: Vec<AuditEntry> = Vec::new();
        for row in rows.flatten() { entries.push(row); }
        entries.reverse();
        entries
    }
}

#[async_trait::async_trait]
impl Middleware for PersistentAuditLogger {
    async fn before_execute(&self, _tool_id: &ToolId, _params: &serde_json::Value) -> Result<(), KernelError> {
        Ok(())
    }

    async fn after_execute(&self, tool_id: &ToolId, output: &mut ToolOutput) -> Result<(), KernelError> {
        let db = self.db.lock().await;
        db.execute(
            "INSERT INTO audit_log (tool_id, success, latency_ms, timestamp) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                tool_id.0,
                output.success as i32,
                output.latency_ms as i64,
                chrono::Utc::now().to_rfc3339(),
            ],
        ).map_err(|e| KernelError::Other(format!("audit log insert failed: {e}")))?;
        // Trim to max_entries
        db.execute(
            "DELETE FROM audit_log WHERE id NOT IN (SELECT id FROM audit_log ORDER BY id DESC LIMIT ?1)",
            [self.max_entries as i64],
        ).ok();
        Ok(())
    }

    fn name(&self) -> &str { "persistent_audit_logger" }
}

// ─── Epistemic Guard ────────────────────────────────────────────────

/// 认识论约束中间件 — Degradation 信号强制 + 违规计数 + 显式声明
///
/// ## 场景
/// kb.* 工具返回 DegradationLevel 时，向 output 注入强制标记，
/// CoreLoop 将其注入下轮 system prompt。
/// 累计连续违规次数，超阈值时生成显式声明插入输出。
///
/// ## 生命周期
/// - 创建：CoreLoop 初始化时注册到 MagChain
/// - 存活：整个 session
/// - violation_count 在 session 内累积，不跨 session
pub struct EpistemicGuard {
    /// 连续违规次数（kb 返回 ZeroHit 后 LLM 仍未调用工具验证）
    violation_count: RwLock<u32>,
    /// 连续 ZeroHit 计数（用于冷启动检测）
    zero_hit_streak: RwLock<u32>,
    /// 显式声明阈值：连续违规超过此值时强制插入声明
    declaration_threshold: u32,
}

impl EpistemicGuard {
    pub fn new() -> Self {
        Self {
            violation_count: RwLock::new(0),
            zero_hit_streak: RwLock::new(0),
            declaration_threshold: 3,
        }
    }

    /// 获取当前违规计数
    pub async fn violations(&self) -> u32 {
        *self.violation_count.read().await
    }

    /// 获取是否处于冷启动状态（连续 ZeroHit 率高）
    pub async fn is_cold_start(&self) -> bool {
        *self.zero_hit_streak.read().await >= 3
    }

    /// 重置违规计数（用户显式确认后）
    pub async fn reset_violations(&self) {
        *self.violation_count.write().await = 0;
    }

    /// 记录一次违规（由 post_check 调用）
    pub async fn record_violation(&self) {
        let mut count = self.violation_count.write().await;
        *count += 1;
    }

    /// 生成显式声明文本（当违规超阈值时，CoreLoop 插入到 LLM 输出前）
    pub async fn declaration_if_needed(&self) -> Option<String> {
        let count = *self.violation_count.read().await;
        if count >= self.declaration_threshold {
            Some(format!(
                "[EPISTEMIC VIOLATION ×{}] 本 session 已连续 {} 次违反认识论约束。\
                后续输出将强制标注 [unverified]，直到调用工具验证或 KB 命中。",
                count, count
            ))
        } else {
            None
        }
    }
}

impl Default for EpistemicGuard {
    fn default() -> Self { Self::new() }
}

#[async_trait::async_trait]
impl Middleware for EpistemicGuard {
    async fn before_execute(&self, _tool_id: &ToolId, _params: &serde_json::Value) -> Result<(), KernelError> {
        Ok(())
    }

    async fn after_execute(&self, tool_id: &ToolId, output: &mut ToolOutput) -> Result<(), KernelError> {
        // 仅处理 kb.* 工具的返回结果
        if !tool_id.0.starts_with("kb_") {
            return Ok(());
        }

        // 提取 degradation level
        let level = output.output
            .get("degradation")
            .and_then(|d| d.get("level"))
            .and_then(|l| l.as_str())
            .unwrap_or("Normal");

        match level {
            "ZeroHit" => {
                // 累计 ZeroHit streak
                let mut streak = self.zero_hit_streak.write().await;
                *streak += 1;

                // 向 output 注入强制约束标记
                if let Some(obj) = output.output.as_object_mut() {
                    obj.insert("_epistemic_constraint".into(), serde_json::json!({
                        "action": "BLOCK_WEIGHT_OUTPUT",
                        "message": "本次查询无结果。禁止从训练权重生成答案，必须声明‘无相关知识’或调用 web.search 验证。",
                        "cold_start": *streak >= 3,
                    }));
                }
            }
            "WeakSignal" => {
                // 重置 ZeroHit streak
                *self.zero_hit_streak.write().await = 0;

                if let Some(obj) = output.output.as_object_mut() {
                    obj.insert("_epistemic_constraint".into(), serde_json::json!({
                        "action": "MARK_REFERENCE_ONLY",
                        "message": "低置信度结果，引用时必须标注 [仅参考]。",
                    }));
                }
            }
            _ => {
                // Normal — 重置 streak
                *self.zero_hit_streak.write().await = 0;
            }
        }

        Ok(())
    }

    fn name(&self) -> &str { "epistemic_guard" }
}

// ─── V29.13 段3c：Hook Visibility Middleware ──────────────────────────────
//
// ## 设计目的
// 让 LLM 在工具输出层面"感知"到 MagChain/Hook 系统在工作。
//
// 当 EpistemicGuard 处于"激活态"（violations>0 或 cold_start=true）时，
// 在 ToolOutput 的对象上追加 `_active_hooks` 字段——LLM 看到这个字段就知道：
// 1. Hook 系统是真实存在的执行层
// 2. 当前有活跃的约束在影响下游决策
// 3. 可以调用 `magchain_status` 工具查询完整状态
//
// ## 为什么不无条件注入
// 每次 ToolOutput +50 字节会污染上下文（每轮 5 个工具调用 = 250 字节）；
// 仅在"有意义可观察"时注入更合适。
//
// ## 引用关系
// - 上游：ToolExecutor 调用 MagChain.after()，触发本 middleware
// - 持有：Arc<EpistemicGuard> 共享同一全局 guard 实例
// - 下游：直接修改 ToolOutput.output 对象
pub struct HookVisibilityMiddleware {
    pub guard: Arc<EpistemicGuard>,
}

#[async_trait::async_trait]
impl Middleware for HookVisibilityMiddleware {
    async fn before_execute(&self, _tool_id: &ToolId, _params: &serde_json::Value) -> Result<(), KernelError> {
        Ok(())
    }

    async fn after_execute(&self, _tool_id: &ToolId, output: &mut ToolOutput) -> Result<(), KernelError> {
        let violations = self.guard.violations().await;
        let cold_start = self.guard.is_cold_start().await;
        // 仅在 hook 真正"激活"时注入——避免普通工具调用每次都添加无意义字段
        if violations == 0 && !cold_start {
            return Ok(());
        }
        if let Some(obj) = output.output.as_object_mut() {
            obj.insert("_active_hooks".into(), serde_json::json!({
                "magchain": "active",
                "epistemic_violations": violations,
                "cold_start": cold_start,
                "guidance": "查询完整状态可调用 `magchain_status` 工具；当前有活跃约束影响输出。",
            }));
        }
        Ok(())
    }

    fn name(&self) -> &str { "hook_visibility" }
}

// ─── Decay Router ───────────────────────────────────────────────────

/// 衰减分流分类
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DecayTier {
    /// API/版本/价格/法规 — 必须 web-first
    Fast,
    /// 技术栈/框架/最佳实践 — weight-first + verify
    Medium,
    /// 算法/数学/语言规则 — weight-first
    Slow,
}

/// 衰减分流路由器
///
/// ## 场景
/// 根据用户输入检测知识类型，调整 Tool Schema 暴露顺序。
/// CoreLoop 在 PromptAssembly 前调用，根据结果调整 web.search 的位置。
pub struct DecayRouter;

impl DecayRouter {
    /// 检测输入文本的衰减层级
    pub fn classify(input: &str) -> DecayTier {
        let lower = input.to_lowercase();

        // 快衰信号词
        let fast_signals = [
            "版本", "version", "最新", "latest", "api", "价格", "price",
            "已废弃", "deprecated", "法规", "合规", "compliance",
            "变更", "changelog", "release", "更新了", "当前",
            // 时间表达 — 年份
            "2024", "2025", "2026",
            // 时间表达 — 英文
            "today", "yesterday", "this week", "this month", "recent", "now", "currently",
            // 时间表达 — 中文
            "今天", "昨天", "本周", "最近", "目前", "最新版",
        ];
        // 慢衰信号词
        let slow_signals = [
            "算法", "algorithm", "复杂度", "complexity", "定理", "theorem",
            "原理", "principle", "语法", "syntax", "数学", "math",
            "物理", "physics", "公理", "axiom",
        ];

        if fast_signals.iter().any(|s| lower.contains(s)) {
            return DecayTier::Fast;
        }
        if slow_signals.iter().any(|s| lower.contains(s)) {
            return DecayTier::Slow;
        }
        DecayTier::Medium
    }

    /// 根据衰减层级返回应提升的工具 ID
    pub fn tools_to_promote(tier: DecayTier) -> Vec<&'static str> {
        match tier {
            DecayTier::Fast => vec!["filengine_web_search", "filengine_web_fetch"],
            DecayTier::Medium => vec![], // 正常排序
            DecayTier::Slow => vec![],   // weight-first，无需提升工具
        }
    }

    /// 生成注入到 system prompt 的衰减提示
    ///
    /// 消费方: CoreLoop PromptAssembly — 拼接到 system prompt 尾部
    pub fn prompt_hint(tier: DecayTier) -> Option<&'static str> {
        match tier {
            DecayTier::Fast => {
                Some("[MUST search first — this query requires up-to-date information]")
            }
            DecayTier::Medium => {
                Some("[Verify claims against current sources if uncertain]")
            }
            DecayTier::Slow => None, // weight-first，无额外提示
        }
    }
}

// ─── Epistemic Post-Check ───────────────────────────────────────────

/// 认识论违规类型
#[derive(Debug, Clone, PartialEq)]
pub enum EpistemicViolation {
    /// kb 返回 ZeroHit 后 LLM 仍从权重输出了事实性断言
    ZeroHitBypass,
    /// 快衰问题未调用 web.search
    FastDecayNoWebSearch,
    /// 输出包含信号词但未标注来源
    UnmarkedFactualClaim,
}

/// Post-Turn 认识论审计器
///
/// ## 场景
/// CoreLoop 在 LLM 输出完成后调用，检查是否违反了 epistemic 约束。
/// 违规记录到 EpistemicGuard，超阈值时触发显式声明。
pub struct EpistemicPostCheck;

impl EpistemicPostCheck {
    /// 检查 LLM 输出是否违反 epistemic 约束
    ///
    /// - `llm_output`: LLM 最终输出文本
    /// - `had_zero_hit`: 本轮是否有 kb.query 返回 ZeroHit
    /// - `decay_tier`: 本轮检测到的衰减层级
    /// - `tools_called`: 本轮实际调用的工具 ID 列表
    pub fn check(
        llm_output: &str,
        had_zero_hit: bool,
        decay_tier: DecayTier,
        tools_called: &[String],
    ) -> Vec<EpistemicViolation> {
        let mut violations = Vec::new();

        // 检查 1: ZeroHit 后仍输出事实性内容（未标注）
        if had_zero_hit
            && has_factual_claims(llm_output)
                && !llm_output.contains("[training_snapshot]")
                && !llm_output.contains("无相关知识")
                && !llm_output.contains("需要外部验证")
            {
                violations.push(EpistemicViolation::ZeroHitBypass);
            }

        // 检查 2: 快衰问题未调用 web
        if decay_tier == DecayTier::Fast {
            // 严格匹配 filengine.web.* —— substring contains 模式留下未来命名误伤隐患
            // (mcp/server-web.search 等会被误判为 web 调用)
            let has_web_call = tools_called.iter()
                .any(|t| t == "filengine_web_search" || t == "filengine_web_fetch");
            if !has_web_call && has_factual_claims(llm_output) {
                violations.push(EpistemicViolation::FastDecayNoWebSearch);
            }
        }

        // 检查 3: 输出含信号词但未标注来源
        if has_signal_words(llm_output)
            && !llm_output.contains("[training_snapshot]")
            && !llm_output.contains("[来源")
            && !tools_called.iter().any(|t| t.starts_with("filengine_web_") || t.starts_with("kb_") || t.starts_with("filengine_fs_"))
        {
            violations.push(EpistemicViolation::UnmarkedFactualClaim);
        }

        violations
    }
}

/// 检测输出是否包含事实性断言（含具体版本号/日期）
fn has_factual_claims(text: &str) -> bool {
    use std::sync::OnceLock;
    static VERSION_RE: OnceLock<regex::Regex> = OnceLock::new();
    static DATE_RE: OnceLock<regex::Regex> = OnceLock::new();

    // 三段版本号: v1.2.3 或 1.78.0（排除单纯浮点数 3.5）
    let version_re = VERSION_RE.get_or_init(|| {
        regex::Regex::new(r"\bv?\d+\.\d+\.\d+\b").unwrap()
    });
    // 日期: 2026-05-19, 2026/05/19
    let date_re = DATE_RE.get_or_init(|| {
        regex::Regex::new(r"20\d{2}[-/]\d{2}[-/]\d{2}").unwrap()
    });

    version_re.is_match(text) || date_re.is_match(text)
}

/// 检测输出是否包含信号词（时态词 + 实体状态词，精确匹配避免误触发）
fn has_signal_words(text: &str) -> bool {
    let signals = [
        "当前版本", "最新版", "已废弃", "已移除", "不再支持", "不再维护",
        "currently", "latest version", "deprecated", "no longer", "removed in",
    ];
    signals.iter().any(|s| text.contains(s))
}

// ════════════════════════════════════════════════════════════════════════════
// PipelineHook — Turn 级粗粒度层
// 与 MagChain 工具级细粒度层正交：后者对每个工具调用 before/after，
// 前者覆盖 Turn 开始/结束/Prompt 组装/PostProcess 等宏观阶段
// ════════════════════════════════════════════════════════════════════════════

/// Pipeline 各阶段触发的事件
///
/// ## 设计
/// 用 owned 类型（无生命周期），异步传递无干扰。
/// 每个字段只包含 hook 展示评断所需的最小信息集。
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// Turn 开始前（安全检查通过后）
    TurnStart {
        /// 用户输入文本
        input: String,
        /// 当前 session id
        session_id: String,
    },
    /// System prompt 组装完成
    PromptBuilt {
        /// System prompt 字符数（用于监控 token 占用）
        system_len: usize,
        /// 动态注入内容数（progressive/epistemic/focus 等）
        dynamic_blocks: usize,
    },
    /// 循环内 LLM 完成（每次 LLM 调用后）
    LlmComplete {
        /// 当前循环第几轮
        loop_iter: usize,
        /// 本次 completion tokens
        completion_tokens: u64,
    },
    /// PostProcess 阶段完成（压缩/MapAnalyzer/效能追踪后）
    PostProcess,
    /// Turn-end fan-out 广播（V29.13 段1：统一记忆系统/MagChain/Palace 协同入口）
    ///
    /// ## 语义
    /// post_process 完成、TurnEnd 触发**之前**广播一次，携带充分的 turn 元数据
    /// 让外部 hook 决定是否做 tier migration / palace absorb / stats flush 等"派生工作"。
    ///
    /// ## 与 PostProcess / TurnEnd 的区别
    /// - `PostProcess`：无参数，仅作为"压缩/Map/效能写完了"的信号灯
    /// - `TurnPostFanOut`：携带元数据，让 hook 真能据此做协同决策
    /// - `TurnEnd`：persistence 后的最终事件，hook 此时改不了任何状态
    ///
    /// ## 为什么不在 PostProcess 加参数
    /// 改 PostProcess 字段会破坏现有 hook（LoggingHook 等已 match 该事件）；
    /// 新增独立 variant 是 *additive change*，所有现有 hook 自动 noop。
    TurnPostFanOut {
        /// 当前 turn 编号
        turn_number: u32,
        /// 当前 session id
        session_id: String,
        /// 工具调用次数
        tool_calls: usize,
        /// 全部成功标志（all_success || tool_calls==0）
        all_success: bool,
        /// 本轮是否触发了消息压缩
        was_compressed: bool,
    },
    /// Turn 正常结束（持久化后）
    TurnEnd {
        /// 最终响应文本长度
        response_len: usize,
        /// 工具调用次数
        tool_calls: usize,
        /// 总延迟（ms）
        latency_ms: u64,
        /// 消耗的 completion tokens
        completion_tokens: u64,
    },
}

/// Pipeline Hook 的响应动作
#[derive(Debug)]
pub enum HookAction {
    /// 继续正常执行
    Continue,
    /// 中止当前 turn，返回中止原因给 LLM
    Abort(String),
}

/// Turn 级 Hook — 第二层 hooks 体系（与 MagChain 工具级正交）
///
/// ## 设计分层
/// - MagChain Middleware：每个工具调用前后（before_execute/after_execute）
/// - PipelineHook：覆盖 Turn 宏观阶段（TurnStart/PromptBuilt/TurnEnd 等）
///
/// ## 生命周期
/// - 创建：实现方手动实例化
/// - 注册：`CoreLoop::add_pipeline_hook(priority, hook)`
/// - 触发：`TurnPipeline` 各阶段内 emit
/// - 销毁：随 CoreLoop Drop
#[async_trait::async_trait]
pub trait PipelineHook: Send + Sync {
    /// Hook 名称（用于日志和调试）
    fn name(&self) -> &str;

    /// 响应一个 Pipeline 事件
    ///
    /// 返回 `HookAction::Continue` 继续执行，
    /// 返回 `HookAction::Abort(reason)` 中止当前 turn。
    async fn on_event(&self, event: &PipelineEvent) -> Result<HookAction, KernelError>;

    /// 过滤：只处理指定事件类型（默认全部处理）
    /// 覆盖以优化性能（高频循环内避免冗余调用）
    fn accepts(&self, _event: &PipelineEvent) -> bool { true }
}

/// 内置示例 Hook：事件日志记录（也可用于单元测试）
pub struct LoggingHook {
    pub prefix: String,
}

#[async_trait::async_trait]
impl PipelineHook for LoggingHook {
    fn name(&self) -> &str { &self.prefix }

    async fn on_event(&self, event: &PipelineEvent) -> Result<HookAction, KernelError> {
        match event {
            PipelineEvent::TurnStart { input, session_id } =>
                tracing::info!(hook = self.prefix.as_str(), session = session_id.as_str(), input_len = input.len(), "turn start"),
            PipelineEvent::PromptBuilt { system_len, dynamic_blocks } =>
                tracing::debug!(hook = self.prefix.as_str(), system_len, dynamic_blocks, "prompt built"),
            PipelineEvent::LlmComplete { loop_iter, completion_tokens } =>
                tracing::debug!(hook = self.prefix.as_str(), loop_iter, completion_tokens, "llm complete"),
            PipelineEvent::PostProcess =>
                tracing::debug!(hook = self.prefix.as_str(), "post process"),
            PipelineEvent::TurnPostFanOut { turn_number, session_id, tool_calls, all_success, was_compressed } =>
                tracing::debug!(hook = self.prefix.as_str(), turn = turn_number, session = session_id.as_str(), tool_calls, all_success, was_compressed, "turn post fan-out"),
            PipelineEvent::TurnEnd { response_len, tool_calls, latency_ms, completion_tokens } =>
                tracing::info!(hook = self.prefix.as_str(), response_len, tool_calls, latency_ms, completion_tokens, "turn end"),
        }
        Ok(HookAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_circuit_breaker() {
        let cb = CircuitBreaker::new(3, Duration::from_millis(100));
        let tool_id = ToolId("test".into());

        // 3 failures should open circuit
        for _ in 0..3 {
            let mut out = ToolOutput {
                tool_id: tool_id.clone(), success: false,
                output: serde_json::Value::Null, latency_ms: 0,
                failure_kind: None, try_instead: Vec::new(),
            };
            cb.after_execute(&tool_id, &mut out).await.unwrap();
        }

        // Next call should fail
        assert!(cb.before_execute(&tool_id, &serde_json::Value::Null).await.is_err());

        // After recovery timeout, should recover
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(cb.before_execute(&tool_id, &serde_json::Value::Null).await.is_ok());
    }

    #[tokio::test]
    async fn test_rate_limiter() {
        let rl = RateLimiter::new(2, Duration::from_millis(100));
        let tool_id = ToolId("test".into());

        assert!(rl.before_execute(&tool_id, &serde_json::Value::Null).await.is_ok());
        assert!(rl.before_execute(&tool_id, &serde_json::Value::Null).await.is_ok());
        assert!(rl.before_execute(&tool_id, &serde_json::Value::Null).await.is_err());

        // After window expires, should allow again
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(rl.before_execute(&tool_id, &serde_json::Value::Null).await.is_ok());
    }

    #[tokio::test]
    async fn test_audit_logger() {
        let logger = AuditLogger::new(100);
        let tool_id = ToolId("test".into());

        let mut out = ToolOutput {
            tool_id: tool_id.clone(), success: true,
            output: serde_json::Value::Null, latency_ms: 50,
            failure_kind: None, try_instead: Vec::new(),
        };
        logger.after_execute(&tool_id, &mut out).await.unwrap();

        let entries = logger.entries().await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].success);
    }

    #[tokio::test]
    async fn test_persistent_audit_logger() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("abacus_audit_test_{}.db", std::process::id()));
        // Clean up from previous runs
        let _ = std::fs::remove_file(&path);

        let logger = PersistentAuditLogger::new(path.clone(), 100).unwrap();
        let tool_id = ToolId("test_persist".into());

        let mut out = ToolOutput {
            tool_id: tool_id.clone(), success: true,
            output: serde_json::Value::Null, latency_ms: 42,
            failure_kind: None, try_instead: Vec::new(),
        };
        logger.after_execute(&tool_id, &mut out).await.unwrap();

        let entries = logger.entries().await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].success);
        assert_eq!(entries[0].latency_ms, 42);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_mag_chain() {
        let mut chain = MagChain::new();
        chain.add(Arc::new(AuditLogger::new(100)));
        assert_eq!(chain.len(), 1);
    }

    // ─── V29.13 段3c：HookVisibilityMiddleware ──────────────────────────

    #[tokio::test]
    async fn hook_visibility_skips_when_idle() {
        // violations=0 + cold_start=false → 不注入 _active_hooks（避免污染普通工具）
        let guard = Arc::new(EpistemicGuard::new());
        let mw = HookVisibilityMiddleware { guard };
        let tool_id = ToolId("filengine_fs_read".into());
        let mut out = ToolOutput {
            tool_id: tool_id.clone(), success: true,
            output: serde_json::json!({"content": "hello"}),
            latency_ms: 10,
            failure_kind: None, try_instead: Vec::new(),
        };
        mw.after_execute(&tool_id, &mut out).await.unwrap();
        assert!(out.output.get("_active_hooks").is_none(), "idle 时不应注入");
    }

    #[tokio::test]
    async fn hook_visibility_injects_when_violations_present() {
        let guard = Arc::new(EpistemicGuard::new());
        guard.record_violation().await;
        let mw = HookVisibilityMiddleware { guard: guard.clone() };
        let tool_id = ToolId("filengine_fs_read".into());
        let mut out = ToolOutput {
            tool_id: tool_id.clone(), success: true,
            output: serde_json::json!({"content": "hello"}),
            latency_ms: 10,
            failure_kind: None, try_instead: Vec::new(),
        };
        mw.after_execute(&tool_id, &mut out).await.unwrap();
        let h = out.output.get("_active_hooks").expect("violations>0 应注入");
        assert_eq!(h.get("magchain").and_then(|v| v.as_str()), Some("active"));
        assert_eq!(h.get("epistemic_violations").and_then(|v| v.as_u64()), Some(1));
    }

    #[tokio::test]
    async fn hook_visibility_handles_non_object_output() {
        // 非 object output（如 string / array）不 panic、静默跳过
        let guard = Arc::new(EpistemicGuard::new());
        guard.record_violation().await;
        let mw = HookVisibilityMiddleware { guard };
        let tool_id = ToolId("test".into());
        let mut out = ToolOutput {
            tool_id: tool_id.clone(), success: true,
            output: serde_json::json!("string output"),
            latency_ms: 0,
            failure_kind: None, try_instead: Vec::new(),
        };
        mw.after_execute(&tool_id, &mut out).await.unwrap();
        // 非 object 不应被改写
        assert_eq!(out.output.as_str(), Some("string output"));
    }

    // ─── V29.13 段1：TurnPostFanOut PipelineEvent ────────────────────────

    #[tokio::test]
    async fn turn_post_fanout_event_carries_metadata() {
        // 验证新增的 PipelineEvent variant 字段读取正确
        let event = PipelineEvent::TurnPostFanOut {
            turn_number: 5,
            session_id: "s1".into(),
            tool_calls: 3,
            all_success: true,
            was_compressed: false,
        };
        // pattern match 验证字段可访问
        match &event {
            PipelineEvent::TurnPostFanOut { turn_number, tool_calls, was_compressed, .. } => {
                assert_eq!(*turn_number, 5);
                assert_eq!(*tool_calls, 3);
                assert!(!was_compressed);
            }
            _ => panic!("variant pattern match 失败"),
        }
        // LoggingHook 接受新事件不 panic
        let hook = LoggingHook { prefix: "test".into() };
        let action = hook.on_event(&event).await.unwrap();
        matches!(action, HookAction::Continue);
    }

    #[tokio::test]
    async fn test_epistemic_guard_zero_hit() {
        let guard = EpistemicGuard::new();
        let tool_id = ToolId("kb_query".into());

        let mut out = ToolOutput {
            tool_id: tool_id.clone(),
            success: true,
            output: serde_json::json!({"results": [], "degradation": {"level": "ZeroHit", "resultCount": 0}}),
            latency_ms: 50,
            failure_kind: None, try_instead: Vec::new(),
        };

        guard.after_execute(&tool_id, &mut out).await.unwrap();

        // Should inject _epistemic_constraint
        let constraint = out.output.get("_epistemic_constraint").unwrap();
        assert_eq!(constraint["action"], "BLOCK_WEIGHT_OUTPUT");
        assert_eq!(constraint["cold_start"], false);
    }

    #[tokio::test]
    async fn test_epistemic_guard_cold_start_detection() {
        let guard = EpistemicGuard::new();
        let tool_id = ToolId("kb_query".into());

        // 3 consecutive ZeroHits → cold_start = true
        for _ in 0..3 {
            let mut out = ToolOutput {
                tool_id: tool_id.clone(), success: true,
                output: serde_json::json!({"results": [], "degradation": {"level": "ZeroHit"}}),
                latency_ms: 30,
                failure_kind: None, try_instead: Vec::new(),
            };
            guard.after_execute(&tool_id, &mut out).await.unwrap();
        }

        assert!(guard.is_cold_start().await);
    }

    #[tokio::test]
    async fn test_epistemic_guard_ignores_non_kb() {
        let guard = EpistemicGuard::new();
        let tool_id = ToolId("filengine_fs_read".into());

        let mut out = ToolOutput {
            tool_id: tool_id.clone(), success: true,
            output: serde_json::json!({"content": "hello"}),
            latency_ms: 5,
            failure_kind: None, try_instead: Vec::new(),
        };

        guard.after_execute(&tool_id, &mut out).await.unwrap();
        // Should NOT inject anything
        assert!(out.output.get("_epistemic_constraint").is_none());
    }

    #[tokio::test]
    async fn test_epistemic_guard_declaration() {
        let guard = EpistemicGuard::new();
        // No violations → no declaration
        assert!(guard.declaration_if_needed().await.is_none());

        // 3 violations → declaration
        for _ in 0..3 {
            guard.record_violation().await;
        }
        let decl = guard.declaration_if_needed().await;
        assert!(decl.is_some());
        assert!(decl.unwrap().contains("EPISTEMIC VIOLATION"));
    }

    #[test]
    fn test_decay_router_classify() {
        assert_eq!(DecayRouter::classify("最新的 Rust 版本是多少"), DecayTier::Fast);
        assert_eq!(DecayRouter::classify("快速排序的算法复杂度"), DecayTier::Slow);
        assert_eq!(DecayRouter::classify("如何用 tokio 写异步代码"), DecayTier::Medium);
    }

    #[test]
    fn test_decay_router_promote() {
        let fast_tools = DecayRouter::tools_to_promote(DecayTier::Fast);
        assert!(fast_tools.contains(&"filengine_web_search"));

        let slow_tools = DecayRouter::tools_to_promote(DecayTier::Slow);
        assert!(slow_tools.is_empty());
    }

    #[test]
    fn test_post_check_zero_hit_bypass() {
        let violations = EpistemicPostCheck::check(
            "当前 Rust 版本是 1.78.0，支持 async trait",
            true,  // had_zero_hit
            DecayTier::Medium,
            &[],
        );
        assert!(violations.contains(&EpistemicViolation::ZeroHitBypass));
    }

    #[test]
    fn test_post_check_fast_decay_no_web() {
        let violations = EpistemicPostCheck::check(
            "最新版本是 v2.0.0，发布于 2026-05-01",
            false,
            DecayTier::Fast,
            &[],  // no web.search called
        );
        assert!(violations.contains(&EpistemicViolation::FastDecayNoWebSearch));
    }

    #[test]
    fn test_post_check_compliant_output() {
        let violations = EpistemicPostCheck::check(
            "快速排序的平均时间复杂度是 O(n log n)",
            false,
            DecayTier::Slow,
            &[],
        );
        // No version/date pattern, no signal words → no violations
        assert!(violations.is_empty());
    }
}
