//! ResourcePressureMonitor — automatic backpressure for bounded resources.
//!
//! Registered sources report pressure [0.0, 1.0]. Exceeding thresholds
//! triggers automatic load shedding (compression, eviction).
//! Called once per turn from TurnPipeline::post_process().

use std::sync::Arc;
use tokio::sync::RwLock;

/// A resource that can report pressure and shed load.
#[async_trait::async_trait]
pub trait PressureSource: Send + Sync {
    fn name(&self) -> &str;
    async fn pressure(&self) -> f64;
    async fn shed(&self, target: f64) -> usize;
}

/// Pressure thresholds (configurable via `pressure.*` config keys)
#[derive(Debug, Clone)]
pub struct PressurePolicy {
    pub soft_threshold: f64,
    pub hard_threshold: f64,
    pub reject_threshold: f64,
}

impl Default for PressurePolicy {
    fn default() -> Self {
        Self { soft_threshold: 0.70, hard_threshold: 0.85, reject_threshold: 0.95 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    Normal,
    Elevated,
    Critical,
    Overloaded,
}

impl std::fmt::Display for PressureLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Normal => write!(f, "normal"),
            Self::Elevated => write!(f, "elevated"),
            Self::Critical => write!(f, "critical"),
            Self::Overloaded => write!(f, "overloaded"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PressureAction {
    pub source: String,
    pub level: PressureLevel,
    pub items_shed: usize,
}

pub struct ResourcePressureMonitor {
    sources: RwLock<Vec<Arc<dyn PressureSource>>>,
    policy: PressurePolicy,
}

impl ResourcePressureMonitor {
    pub fn new(policy: PressurePolicy) -> Self {
        Self { sources: RwLock::new(Vec::new()), policy }
    }

    pub async fn register(&self, source: Arc<dyn PressureSource>) {
        self.sources.write().await.push(source);
    }

    pub async fn check_and_shed(&self) -> Vec<PressureAction> {
        let sources = self.sources.read().await;
        let mut actions = Vec::new();
        for source in sources.iter() {
            let p = source.pressure().await;
            let level = self.classify(p);
            match level {
                PressureLevel::Elevated => {
                    let shed = source.shed(self.policy.soft_threshold).await;
                    if shed > 0 {
                        actions.push(PressureAction { source: source.name().into(), level, items_shed: shed });
                    }
                }
                PressureLevel::Critical | PressureLevel::Overloaded => {
                    let shed = source.shed(self.policy.soft_threshold * 0.7).await;
                    actions.push(PressureAction { source: source.name().into(), level, items_shed: shed });
                }
                PressureLevel::Normal => {}
            }
        }
        actions
    }

    pub async fn status(&self) -> Vec<(String, PressureLevel, f64)> {
        let sources = self.sources.read().await;
        let mut result = Vec::new();
        for source in sources.iter() {
            let p = source.pressure().await;
            result.push((source.name().into(), self.classify(p), p));
        }
        result
    }

    fn classify(&self, pressure: f64) -> PressureLevel {
        if pressure >= self.policy.reject_threshold { PressureLevel::Overloaded }
        else if pressure >= self.policy.hard_threshold { PressureLevel::Critical }
        else if pressure >= self.policy.soft_threshold { PressureLevel::Elevated }
        else { PressureLevel::Normal }
    }
}

// ─── Tests ────────────────────────────────────────────────────────
//
// ## 覆盖范围
// - `classify` 4 阈值边界：reject / hard / soft / 其余
// - `check_and_shed` 状态机：Normal 不调 shed / Elevated 仅 shed>0 记 / Critical+Overloaded 无条件记
// - `status` 返回三元组
// - 多源串行处理顺序保留
//
// ## Mock 设计
// `MockSource` 持有 `pressure_value` 和 `shed_return`，记录被调次数 + 调用参数，
// 测试断言可拿到调用次数验证 trait method 是否被触发。
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, AtomicU64, Ordering};

    /// Mock PressureSource — 可控 pressure + 可记 shed 调用。
    /// 创建：每个测试 case 内 `MockSource::new(name, pressure, shed_return)`
    /// 销毁：随测试函数退出
    struct MockSource {
        name: String,
        pressure_value: f64,
        shed_return: usize,
        shed_calls: AtomicUsize,
        last_shed_target: AtomicU64, // f64 通过 to_bits 存
    }

    impl MockSource {
        fn new(name: &str, pressure: f64, shed_return: usize) -> Arc<Self> {
            Arc::new(Self {
                name: name.into(),
                pressure_value: pressure,
                shed_return,
                shed_calls: AtomicUsize::new(0),
                last_shed_target: AtomicU64::new(0),
            })
        }
        fn shed_call_count(&self) -> usize {
            self.shed_calls.load(Ordering::SeqCst)
        }
        fn last_shed_target_f64(&self) -> f64 {
            f64::from_bits(self.last_shed_target.load(Ordering::SeqCst))
        }
    }

    #[async_trait::async_trait]
    impl PressureSource for MockSource {
        fn name(&self) -> &str { &self.name }
        async fn pressure(&self) -> f64 { self.pressure_value }
        async fn shed(&self, target: f64) -> usize {
            self.shed_calls.fetch_add(1, Ordering::SeqCst);
            self.last_shed_target.store(target.to_bits(), Ordering::SeqCst);
            self.shed_return
        }
    }

    fn monitor() -> ResourcePressureMonitor {
        ResourcePressureMonitor::new(PressurePolicy::default())
    }

    // ─── classify 阈值矩阵 ─────────────────────────────────

    #[test]
    fn classify_normal_below_soft() {
        let m = monitor();
        assert_eq!(m.classify(0.0), PressureLevel::Normal);
        assert_eq!(m.classify(0.69), PressureLevel::Normal);
    }

    #[test]
    fn classify_elevated_at_soft_threshold() {
        let m = monitor();
        // 0.70 == soft_threshold → 进 Elevated（>= 而非 >）
        assert_eq!(m.classify(0.70), PressureLevel::Elevated);
        assert_eq!(m.classify(0.84), PressureLevel::Elevated);
    }

    #[test]
    fn classify_critical_at_hard_threshold() {
        let m = monitor();
        assert_eq!(m.classify(0.85), PressureLevel::Critical);
        assert_eq!(m.classify(0.94), PressureLevel::Critical);
    }

    #[test]
    fn classify_overloaded_at_reject_threshold() {
        let m = monitor();
        assert_eq!(m.classify(0.95), PressureLevel::Overloaded);
        assert_eq!(m.classify(1.0), PressureLevel::Overloaded);
        // > 1.0 也归 Overloaded（无上限 clamp）
        assert_eq!(m.classify(2.0), PressureLevel::Overloaded);
    }

    // ─── check_and_shed 行为 ──────────────────────────────

    #[tokio::test]
    async fn check_and_shed_normal_does_not_call_shed() {
        let m = monitor();
        let src = MockSource::new("src", 0.5, 99);
        m.register(src.clone()).await;
        let actions = m.check_and_shed().await;
        assert!(actions.is_empty());
        assert_eq!(src.shed_call_count(), 0, "Normal must not invoke shed");
    }

    #[tokio::test]
    async fn check_and_shed_elevated_skips_action_when_shed_returns_zero() {
        // Elevated 分支：shed > 0 才记 action
        let m = monitor();
        let src = MockSource::new("src", 0.75, 0); // pressure 触发 Elevated，shed 返回 0
        m.register(src.clone()).await;
        let actions = m.check_and_shed().await;
        assert!(actions.is_empty(), "Elevated + shed=0 → 不记 action");
        assert_eq!(src.shed_call_count(), 1, "shed must still be invoked");
        // shed target 应是 soft_threshold (0.70)
        assert!((src.last_shed_target_f64() - 0.70).abs() < 1e-9);
    }

    #[tokio::test]
    async fn check_and_shed_elevated_records_action_when_shed_returns_positive() {
        let m = monitor();
        let src = MockSource::new("src", 0.80, 5);
        m.register(src.clone()).await;
        let actions = m.check_and_shed().await;
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].source, "src");
        assert_eq!(actions[0].level, PressureLevel::Elevated);
        assert_eq!(actions[0].items_shed, 5);
    }

    #[tokio::test]
    async fn check_and_shed_critical_always_records_action() {
        // Critical 分支：无条件记 action（即使 shed=0），shed target = soft * 0.7
        let m = monitor();
        let src = MockSource::new("src", 0.90, 0);
        m.register(src.clone()).await;
        let actions = m.check_and_shed().await;
        assert_eq!(actions.len(), 1, "Critical must record action even when shed=0");
        assert_eq!(actions[0].level, PressureLevel::Critical);
        assert_eq!(actions[0].items_shed, 0);
        // soft_threshold * 0.7 = 0.49
        assert!((src.last_shed_target_f64() - 0.49).abs() < 1e-9);
    }

    #[tokio::test]
    async fn check_and_shed_overloaded_uses_critical_target() {
        let m = monitor();
        let src = MockSource::new("src", 0.99, 12);
        m.register(src.clone()).await;
        let actions = m.check_and_shed().await;
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].level, PressureLevel::Overloaded);
        assert_eq!(actions[0].items_shed, 12);
        // Overloaded 与 Critical 走同一分支，target 仍是 soft*0.7
        assert!((src.last_shed_target_f64() - 0.49).abs() < 1e-9);
    }

    // ─── 多源 + status ─────────────────────────────────────

    #[tokio::test]
    async fn check_and_shed_processes_multiple_sources_in_registration_order() {
        let m = monitor();
        let s1 = MockSource::new("a", 0.50, 0); // Normal
        let s2 = MockSource::new("b", 0.90, 3); // Critical
        let s3 = MockSource::new("c", 0.99, 7); // Overloaded
        m.register(s1.clone()).await;
        m.register(s2.clone()).await;
        m.register(s3.clone()).await;
        let actions = m.check_and_shed().await;
        // Normal 不记，Critical/Overloaded 各 1 → 2 个 action 按注册顺序
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].source, "b");
        assert_eq!(actions[1].source, "c");
        assert_eq!(s1.shed_call_count(), 0);
        assert_eq!(s2.shed_call_count(), 1);
        assert_eq!(s3.shed_call_count(), 1);
    }

    #[tokio::test]
    async fn status_returns_per_source_snapshot() {
        let m = monitor();
        m.register(MockSource::new("ctx", 0.30, 0)).await;
        m.register(MockSource::new("queue", 0.85, 0)).await;
        let snap = m.status().await;
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].0, "ctx");
        assert_eq!(snap[0].1, PressureLevel::Normal);
        assert!((snap[0].2 - 0.30).abs() < 1e-9);
        assert_eq!(snap[1].0, "queue");
        assert_eq!(snap[1].1, PressureLevel::Critical);
    }

    #[test]
    fn pressure_level_display() {
        assert_eq!(PressureLevel::Normal.to_string(), "normal");
        assert_eq!(PressureLevel::Elevated.to_string(), "elevated");
        assert_eq!(PressureLevel::Critical.to_string(), "critical");
        assert_eq!(PressureLevel::Overloaded.to_string(), "overloaded");
    }

    #[test]
    fn pressure_policy_default_thresholds() {
        let p = PressurePolicy::default();
        assert!((p.soft_threshold - 0.70).abs() < 1e-9);
        assert!((p.hard_threshold - 0.85).abs() < 1e-9);
        assert!((p.reject_threshold - 0.95).abs() < 1e-9);
    }
}
