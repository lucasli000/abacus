//! LlmBudget — cost/token/latency 三维预算作为 PressureSource 接入 ResourcePressureMonitor
//!
//! ## 设计目标（治本路径）
//!
//! 解决"LLM 消耗资源无边界"的核心问题：把 cost/token/latency 三维消耗**显式跟踪**，
//! 当压力超阈值时**主动降级**（shed）到更便宜的 provider，并通过**现有**的
//! `ResourcePressureMonitor` 统一调度，不造新调度体系。
//!
//! ## 为什么是 PressureSource
//!
//! `core/pressure.rs` 已有 `ResourcePressureMonitor`：注册 `PressureSource`，
//! `check_and_shed()` 时遍历每个 source 调 `pressure()` + `shed()`，把
//! "超阈值即降级" 这件事统一收口。**LlmBudget 是这个抽象的天然公民**——
//! 不应再造一个并行的 budget tracker。
//!
//! ## 真实落地（vs 想象）
//!
//! 1. `CoreLoop` 调 LLM 后**真的**调 `LlmBudget::record(cost, tokens, latency)`
//! 2. `CoreLoop` 调 LLM **前**真的调 `pressure_monitor.check_and_shed()` → 如果
//!    LlmBudget 返回 shed action，CoreLoop 切换到 fallback provider
//! 3. `config.toml` 暴露 `[llm_budget]`，用户可配 max_cost_usd / max_tokens
//! 4. TUI 状态栏显示 cost / pressure level（用户**真的能看到**）
//! 5. 集成测试：mock LLM provider 跑 N 次 → 验证 budget 累加、shed 触发
//!
//! ## 不在范围（避免过度设计）
//!
//! - 学习型 priority 调整（reinforcement learning）—— V2
//! - Query 复杂度自动选 model（Abacus 模型名由用户显式指定，不需要）
//! - Critic Agent 用 LLM 评 LLM —— 零成本启发式评分已足够

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use abacus_types::ModelId;
use crate::core::pressure::{PressureSource, PressureLevel};

/// 单次 session 的 LLM 预算配置
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LlmBudgetConfig {
    /// 单次 session 的费用上限（USD），0 = 不限
    pub max_cost_usd: f64,
    /// 单次 session 的 token 上限，0 = 不限
    pub max_total_tokens: u64,
    /// 软警告阈值（pressure ratio），默认 0.7
    pub soft_threshold: f64,
    /// 硬降级阈值（pressure ratio），默认 0.85
    pub hard_threshold: f64,
    /// 拒绝继续阈值（pressure ratio），默认 0.95
    pub reject_threshold: f64,
    /// 最近 N 次 turn 的延迟窗口（用于 P95 计算）
    pub latency_window: usize,
}

impl Default for LlmBudgetConfig {
    fn default() -> Self {
        Self {
            max_cost_usd: 0.0,  // 默认不限（让 opt-in 用户显式开）
            max_total_tokens: 0,
            soft_threshold: 0.70,
            hard_threshold: 0.85,
            reject_threshold: 0.95,
            latency_window: 20,
        }
    }
}

/// 模型成本表（per 1K tokens, USD）
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct ModelCost {
    pub input_per_1k: f64,
    pub output_per_1k: f64,
}

impl ModelCost {
    /// 计算单次调用的 cost
    pub fn compute(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        (input_tokens as f64 / 1000.0) * self.input_per_1k
            + (output_tokens as f64 / 1000.0) * self.output_per_1k
    }
}

/// 单次 LLM 调用的实际消耗记录
#[derive(Debug, Clone)]
pub struct LlmUsage {
    pub cost_usd: f64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency: Duration,
    pub model: ModelId,
}

impl LlmUsage {
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// LlmBudget 实时状态
#[derive(Debug, Clone, Default)]
struct BudgetState {
    cost_usd: f64,
    total_tokens: u64,
    call_count: u32,
    latencies: VecDeque<Duration>,
    /// 最近一次降级：标记已 shed 过，避免重复 shed
    last_shed_at: Option<std::time::Instant>,
}

/// LLM 资源预算 — 实现 `PressureSource` 接入 `ResourcePressureMonitor`
///
/// ## 关键点
///
/// - 内部用 `Arc<RwLock<BudgetState>>` 共享给 ProviderRegistry / CoreLoop
/// - `pressure()` 返回 cost 与 max 的比率（若有 max；max=0 则按 token）
/// - `shed(target)` 返回 1（成功降级）或 0（无需降级 / 已降过）
/// - **shed 副作用**：标记"已建议降级"，让 `ProviderRegistry` 切到 cheaper provider
/// - `config` 用 parking_lot RwLock 支持运行时 reconfigure（用户改 config.toml 后热加载）
pub struct LlmBudget {
    config: parking_lot::RwLock<LlmBudgetConfig>,
    state: Arc<RwLock<BudgetState>>,
    /// 成本表：model_id → per-token cost（外部注入）
    cost_table: Arc<RwLock<std::collections::HashMap<ModelId, ModelCost>>>,
    /// shed 回调：被 shed() 调用时通知 CoreLoop 切 fallback provider
    shed_notifier: Arc<RwLock<Option<Box<dyn Fn() -> usize + Send + Sync>>>>,
}

impl LlmBudget {
    pub fn new(config: LlmBudgetConfig) -> Self {
        Self {
            config: parking_lot::RwLock::new(config),
            state: Arc::new(RwLock::new(BudgetState {
                latencies: VecDeque::new(),
                ..Default::default()
            })),
            cost_table: Arc::new(RwLock::new(std::collections::HashMap::new())),
            shed_notifier: Arc::new(RwLock::new(None)),
        }
    }

    /// 热重载 config（用户改 config.toml 后调用）
    pub fn reconfigure(&self, new_config: LlmBudgetConfig) {
        *self.config.write() = new_config;
        tracing::info!("LlmBudget reconfigured");
    }

    /// 注册模型成本（启动时一次性灌入）
    pub async fn register_model_cost(&self, model: ModelId, cost: ModelCost) {
        self.cost_table.write().await.insert(model, cost);
    }

    /// 注册 shed 回调（CoreLoop 在 shed 时调，切到 cheaper provider）
    pub async fn on_shed<F>(&self, f: F)
    where
        F: Fn() -> usize + Send + Sync + 'static,
    {
        *self.shed_notifier.write().await = Some(Box::new(f));
    }

    /// **真实落地钩子**：CoreLoop 调 LLM 后调用，记录实际消耗
    pub async fn record(&self, usage: LlmUsage) {
        let mut s = self.state.write().await;
        let latency_window = self.config.read().latency_window;
        s.cost_usd += usage.cost_usd;
        s.total_tokens += usage.total_tokens();
        s.call_count += 1;
        s.latencies.push_back(usage.latency);
        while s.latencies.len() > latency_window {
            s.latencies.pop_front();
        }
    }

    /// 计算 cost（用注入的 cost table + token 计数）
    pub async fn compute_cost(&self, model: &ModelId, prompt_tokens: u64, completion_tokens: u64) -> f64 {
        let table = self.cost_table.read().await;
        if let Some(c) = table.get(model) {
            c.compute(prompt_tokens, completion_tokens)
        } else {
            // 未知 model → 保守估 0（避免未注册 model 让用户付"未知"费）
            tracing::debug!("model {:?} not in cost table; cost=0", model);
            0.0
        }
    }

    /// 综合压力：cost% vs token% vs latency P95% 的最大值
    pub async fn pressure_ratio(&self) -> f64 {
        let s = self.state.read().await;
        let cfg = self.config.read();
        let cost_p = if cfg.max_cost_usd > 0.0 {
            s.cost_usd / cfg.max_cost_usd
        } else {
            0.0
        };
        let token_p = if cfg.max_total_tokens > 0 {
            s.total_tokens as f64 / cfg.max_total_tokens as f64
        } else {
            0.0
        };
        // latency P95
        let lat_p = if s.latencies.is_empty() {
            0.0
        } else {
            let mut sorted: Vec<Duration> = s.latencies.iter().copied().collect();
            sorted.sort();
            let p95 = sorted[sorted.len() * 95 / 100];
            // 用 60s 基准（任何 turn 超过 60s 视为 100% 压力）
            (p95.as_secs_f64() / 60.0).min(2.0)
        };
        cost_p.max(token_p).max(lat_p).clamp(0.0, 2.0)
    }

    /// 当前压力等级
    pub async fn level(&self) -> PressureLevel {
        let p = self.pressure_ratio().await;
        let cfg = self.config.read();
        if p >= cfg.reject_threshold { PressureLevel::Overloaded }
        else if p >= cfg.hard_threshold { PressureLevel::Critical }
        else if p >= cfg.soft_threshold { PressureLevel::Elevated }
        else { PressureLevel::Normal }
    }

    /// 状态快照（用于 TUI /status 显示）
    pub async fn snapshot(&self) -> BudgetSnapshot {
        let s = self.state.read().await;
        let p = self.pressure_ratio().await;
        let level = self.level().await;
        let cfg = self.config.read();
        let avg_latency = if s.latencies.is_empty() {
            Duration::ZERO
        } else {
            let total: Duration = s.latencies.iter().sum();
            total / s.latencies.len() as u32
        };
        BudgetSnapshot {
            cost_usd: s.cost_usd,
            max_cost_usd: cfg.max_cost_usd,
            total_tokens: s.total_tokens,
            max_total_tokens: cfg.max_total_tokens,
            call_count: s.call_count,
            avg_latency,
            pressure_ratio: p,
            level,
        }
    }

    /// 是否应该拒绝继续（overloaded）
    pub async fn should_halt(&self) -> bool {
        self.level().await == PressureLevel::Overloaded
    }

    /// 标记"已 shed"（shed callback 调用后）
    async fn mark_shed(&self) {
        let mut s = self.state.write().await;
        s.last_shed_at = Some(std::time::Instant::now());
    }
}

/// TUI / 日志用的快照
#[derive(Debug, Clone)]
pub struct BudgetSnapshot {
    pub cost_usd: f64,
    pub max_cost_usd: f64,
    pub total_tokens: u64,
    pub max_total_tokens: u64,
    pub call_count: u32,
    pub avg_latency: Duration,
    pub pressure_ratio: f64,
    pub level: PressureLevel,
}

impl std::fmt::Display for BudgetSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f,
            "cost=${:.4}/{:.2} tokens={}/{} calls={} avg_lat={}ms level={} pressure={:.0}%",
            self.cost_usd, self.max_cost_usd,
            self.total_tokens, self.max_total_tokens,
            self.call_count,
            self.avg_latency.as_millis(),
            self.level,
            self.pressure_ratio * 100.0
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// PressureSource 实现 — 融入 ResourcePressureMonitor
// ═══════════════════════════════════════════════════════════════════════════════

#[async_trait::async_trait]
impl PressureSource for LlmBudget {
    fn name(&self) -> &str { "llm_budget" }

    async fn pressure(&self) -> f64 {
        self.pressure_ratio().await
    }

    async fn shed(&self, target: f64) -> usize {
        // target = 要降到的压力水平（如 0.7）
        let p = self.pressure_ratio().await;
        let hard = self.config.read().hard_threshold;
        if p < hard {
            return 0; // 还不到降级门槛
        }
        // 已 shed 过 → 60s 内不重复（避免抖动）
        {
            let s = self.state.read().await;
            if let Some(t) = s.last_shed_at {
                if t.elapsed() < Duration::from_secs(60) {
                    return 0;
                }
            }
        }
        // 调用 shed callback 切到 cheaper provider
        let cb = self.shed_notifier.read().await;
        let switched = if let Some(f) = cb.as_ref() {
            f()
        } else {
            tracing::warn!("LlmBudget shed: no notifier registered; cannot switch provider");
            0
        };
        if switched > 0 {
            tracing::warn!(
                "LlmBudget shed: pressure={:.2} target={:.2} switched_to_fallback={}",
                p, target, switched
            );
            self.mark_shed().await;
        }
        switched
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LlmBudgetConfig {
        LlmBudgetConfig {
            max_cost_usd: 1.0,
            max_total_tokens: 100_000,
            soft_threshold: 0.7,
            hard_threshold: 0.85,
            reject_threshold: 0.95,
            latency_window: 10,
        }
    }

    fn usage(model: &str, cost: f64, in_t: u64, out_t: u64, lat_ms: u64) -> LlmUsage {
        LlmUsage {
            cost_usd: cost,
            prompt_tokens: in_t,
            completion_tokens: out_t,
            latency: Duration::from_millis(lat_ms),
            model: ModelId(model.into()),
        }
    }

    #[tokio::test]
    async fn budget_starts_comfortable() {
        let b = LlmBudget::new(cfg());
        let snap = b.snapshot().await;
        assert_eq!(snap.level, PressureLevel::Normal);
        assert_eq!(snap.call_count, 0);
    }

    #[tokio::test]
    async fn cost_accumulates_proportionally() {
        let b = LlmBudget::new(cfg());
        b.record(usage("m1", 0.3, 30_000, 10_000, 100)).await;
        b.record(usage("m1", 0.4, 40_000, 20_000, 200)).await;
        let snap = b.snapshot().await;
        assert!((snap.cost_usd - 0.7).abs() < 1e-6);
        assert_eq!(snap.total_tokens, 100_000);
        assert_eq!(snap.call_count, 2);
        // cost 0.7/1.0 = 70% (Elevated), tokens 100k/100k = 100% (Overloaded)
        // 综合压力 = max → Overloaded
        assert_eq!(snap.level, PressureLevel::Overloaded);
    }

    #[tokio::test]
    async fn over_reject_threshold_marks_overloaded() {
        let b = LlmBudget::new(cfg());
        b.record(usage("m1", 1.5, 0, 0, 100)).await;
        assert!(b.should_halt().await);
        assert_eq!(b.level().await, PressureLevel::Overloaded);
    }

    #[tokio::test]
    async fn unlimited_budget_never_overloaded() {
        let mut c = cfg();
        c.max_cost_usd = 0.0;  // 不限
        c.max_total_tokens = 0;
        let b = LlmBudget::new(c);
        b.record(usage("m1", 999.0, 9_999_999, 9_999_999, 100)).await;
        let snap = b.snapshot().await;
        assert_eq!(snap.level, PressureLevel::Normal);
        assert!(!b.should_halt().await);
    }

    #[tokio::test]
    async fn shed_returns_0_when_below_hard_threshold() {
        let b = LlmBudget::new(cfg());
        // 50% pressure — 不到 hard_threshold
        b.record(usage("m1", 0.5, 0, 0, 100)).await;
        let n = b.shed(0.7).await;
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn shed_calls_notifier_when_over_hard_threshold() {
        let b = Arc::new(LlmBudget::new(cfg()));
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = counter.clone();
        b.on_shed(move || {
            counter_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            1  // switched to 1 fallback
        }).await;
        // 推到 hard_threshold 之上
        b.record(usage("m1", 0.95, 0, 0, 100)).await;
        let n = b.shed(0.7).await;
        assert_eq!(n, 1);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn shed_cooldown_prevents_repeat_within_60s() {
        let b = Arc::new(LlmBudget::new(cfg()));
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = counter.clone();
        b.on_shed(move || {
            counter_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            1
        }).await;
        b.record(usage("m1", 0.95, 0, 0, 100)).await;
        assert_eq!(b.shed(0.7).await, 1);
        // 60s 内再 shed → cooldown 阻止
        assert_eq!(b.shed(0.7).await, 0);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn model_cost_computation() {
        let b = LlmBudget::new(cfg());
        let cost = ModelCost { input_per_1k: 0.003, output_per_1k: 0.015 };  // GPT-4o-ish
        b.register_model_cost(ModelId("gpt-4o".into()), cost).await;
        let c = b.compute_cost(&ModelId("gpt-4o".into()), 1000, 500).await;
        // 1000/1000 * 0.003 + 500/1000 * 0.015 = 0.003 + 0.0075 = 0.0105
        assert!((c - 0.0105).abs() < 1e-6);
        // 未注册的 model → 0
        let c = b.compute_cost(&ModelId("unknown".into()), 1000, 500).await;
        assert_eq!(c, 0.0);
    }

    #[tokio::test]
    async fn snapshot_display_format() {
        let b = LlmBudget::new(cfg());
        b.record(usage("m1", 0.5, 50_000, 20_000, 150)).await;
        let snap = b.snapshot().await;
        let s = format!("{snap}");
        eprintln!("SNAPSHOT: {s}");
        assert!(s.contains("cost=$0.5000/1.00"), "missing cost in: {s}");
        assert!(s.contains("calls=1"), "missing calls in: {s}");
        assert!(s.contains("level=elevated"), "missing level in: {s}");
    }

    /// **真实落地验证**（vs 想象）：注册到 ResourcePressureMonitor 后，调用
    /// monitor.check_and_shed() 真正会触发 LlmBudget::shed()
    #[tokio::test]
    async fn integration_with_resource_pressure_monitor() {
        use crate::core::pressure::{ResourcePressureMonitor, PressurePolicy};

        let budget = Arc::new(LlmBudget::new(cfg()));
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = counter.clone();
        budget.on_shed(move || { c.fetch_add(1, std::sync::atomic::Ordering::SeqCst); 1 }).await;
        budget.record(usage("m1", 0.95, 0, 0, 100)).await;  // 推到 hard+

        let monitor = ResourcePressureMonitor::new(PressurePolicy::default());
        monitor.register(budget.clone()).await;

        let actions = monitor.check_and_shed().await;
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].source, "llm_budget");
        assert!(matches!(actions[0].level, PressureLevel::Critical | PressureLevel::Overloaded));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
