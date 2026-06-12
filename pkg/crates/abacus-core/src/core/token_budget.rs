//! TokenBudgetMonitor — 实时 Token 预算管理
//!
//! ## 职责
//! 在 pipeline 构建 LlmRequest 前评估 context 使用率，返回 PressureLevel
//! 指导工具列表裁剪力度和压缩策略。
//!
//! ## 引用关系
//! - 创建: CoreLoop::new()（随 CoreConfig 初始化）
//! - 消费方: TurnPipeline execute_loop（每轮构建 request 前查询 pressure_level）
//! - 数据源: ContextManager.window.current_tokens / max_tokens
//!
//! ## 生命周期
//! - 创建：CoreLoop 初始化
//! - 读取：每轮 turn 开始（pipeline setup 阶段）
//! - 销毁：随 CoreLoop drop

/// Token 压力等级（决定工具列表裁剪力度）
///
/// ## 消费方
/// - TurnPipeline: 根据 level 选择 build_tool_definitions 的变体
/// - ContextManager: Critical 时触发 force_compress
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    /// < 60% 使用率：正常模式，完整 description + 全部工具
    Normal,
    /// 60-80%：温和模式，冷工具用 short_description
    Warm,
    /// 80-90%：积极模式，frequency pruning 阈值降为 10 turns
    Hot,
    /// > 90%：激进模式，仅保留 top-5 热点工具 + force compress
    Critical,
}

/// Token 预算分配（各组件占比）
#[derive(Debug, Clone)]
pub struct BudgetAllocation {
    /// System prompt 占比（默认 25%）
    pub system_prompt_pct: f64,
    /// Tool definitions 占比（默认 15%）
    pub tools_pct: f64,
    /// Messages/context 占比（默认 55%）
    pub messages_pct: f64,
    /// 安全余量（默认 5%）
    pub reserve_pct: f64,
}

impl Default for BudgetAllocation {
    fn default() -> Self {
        Self {
            system_prompt_pct: 0.25,
            tools_pct: 0.15,
            messages_pct: 0.55,
            reserve_pct: 0.05,
        }
    }
}

/// Token 预算监控器
///
/// ## 设计
/// 无状态查询——每次调用 pressure_level() 传入当前使用率即可。
/// 不持有 ContextManager 引用（避免循环依赖），由 pipeline 中介调用。
#[derive(Debug, Clone)]
pub struct TokenBudgetMonitor {
    /// Context window 总容量（tokens）
    pub max_tokens: usize,
    /// 各组件预算分配
    pub budget: BudgetAllocation,
}

impl TokenBudgetMonitor {
    pub fn new(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            budget: BudgetAllocation::default(),
        }
    }

    /// 根据当前使用率返回压力等级
    ///
    /// 2026-06-11 调整: Hot 80-90% → 85-95%, Critical > 90% → > 95%
    /// 缓解长任务 context 累积时的过早 Critical 警告
    pub fn pressure_level(&self, current_tokens: usize) -> PressureLevel {
        let usage = current_tokens as f64 / self.max_tokens as f64;
        match usage {
            x if x < 0.6 => PressureLevel::Normal,
            x if x < 0.85 => PressureLevel::Warm,
            x if x < 0.95 => PressureLevel::Hot,
            _ => PressureLevel::Critical,
        }
    }

    /// 工具 definitions 的 token 预算上限
    pub fn tools_budget(&self) -> usize {
        (self.max_tokens as f64 * self.budget.tools_pct) as usize
    }

    /// Messages 的 token 预算上限
    pub fn messages_budget(&self) -> usize {
        (self.max_tokens as f64 * self.budget.messages_pct) as usize
    }

    /// System prompt 的 token 预算上限
    pub fn system_budget(&self) -> usize {
        (self.max_tokens as f64 * self.budget.system_prompt_pct) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_level_boundaries() {
        let monitor = TokenBudgetMonitor::new(100_000);
        assert_eq!(monitor.pressure_level(50_000), PressureLevel::Normal);
        assert_eq!(monitor.pressure_level(60_000), PressureLevel::Warm);
        assert_eq!(monitor.pressure_level(80_000), PressureLevel::Warm);
        assert_eq!(monitor.pressure_level(85_000), PressureLevel::Hot);
        assert_eq!(monitor.pressure_level(94_000), PressureLevel::Hot);
        assert_eq!(monitor.pressure_level(95_000), PressureLevel::Critical);
        assert_eq!(monitor.pressure_level(99_000), PressureLevel::Critical);
    }

    #[test]
    fn budget_allocation_defaults() {
        let monitor = TokenBudgetMonitor::new(128_000);
        assert_eq!(monitor.tools_budget(), 19200); // 15% of 128K
        assert_eq!(monitor.messages_budget(), 70400); // 55% of 128K
        assert_eq!(monitor.system_budget(), 32000); // 25% of 128K
    }

    #[test]
    fn zero_tokens_is_normal() {
        let monitor = TokenBudgetMonitor::new(100_000);
        assert_eq!(monitor.pressure_level(0), PressureLevel::Normal);
    }
}
