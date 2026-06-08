//! Animation hooks —— 复用的视觉节拍 helper
//!
//! ## 设计目标
//!
//! 把散落在各处的"时间节拍计算"模式收口成命名清晰的 struct/fn, 跨 Section 复用。
//! 解决之前的"Cell<u64> 漂移 + subsec_millis() 公式散落"问题。
//!
//! ## 包含的 hooks
//!
//! - [`SpinnerFrame`] —— Braille 10 帧旋转 spinner（按 100ms/150ms 节拍）
//! - [`PulseGate`] —— 周期性布尔脉冲（用于 BOLD 加粗切换）
//! - [`ShimmerPhase`] —— 光带 phase 计算（按 tick + 周期）
//!
//! ## 设计原则
//!
//! - **无内部状态** —— 所有 helper 是纯函数 / 计算器, 节拍来源由调用方提供（SystemTime 或 anim_tick）
//! - **跨 SectionContext 通用** —— 不依赖具体 state 字段, 输入是数字, 输出是数字 / 字符
//! - **可单元测试** —— 喂固定 tick 值即可断言输出

use std::time::SystemTime;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// SpinnerFrame —— Braille 10 帧旋转 spinner
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Braille spinner 字符集 —— 10 帧
pub const SPINNER_FRAMES: [&str; 10] = [
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
];

/// 计算当前 spinner 帧
///
/// ## 参数
/// - `interval_ms`: 切帧间隔毫秒数. 推荐 100（流畅）或 150（节能）
///
/// ## 实现
/// 按 wall-clock 时间取模, 不依赖 frame counter, 避免渲染丢帧导致 spinner 停顿
///
/// ## 用法示例
/// ```ignore
/// use abacus_ui_kit::hooks::SpinnerFrame;
///
/// let frame = SpinnerFrame::current(100); // 100ms 切帧
/// ```
pub struct SpinnerFrame;

impl SpinnerFrame {
    /// 取当前 spinner 字符（按 wall-clock ms 模 SPINNER_FRAMES.len()）
    pub fn current(interval_ms: u64) -> &'static str {
        Self::current_at(now_subsec_millis(), interval_ms)
    }

    /// 测试友好版本: 显式传入 ms（用于断言）
    pub fn current_at(now_ms: u64, interval_ms: u64) -> &'static str {
        let interval = interval_ms.max(1);
        let idx = ((now_ms / interval) % SPINNER_FRAMES.len() as u64) as usize;
        SPINNER_FRAMES[idx]
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// PulseGate —— 周期性布尔脉冲
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 周期性布尔脉冲 —— 按固定 ms 周期 on/off 切换
///
/// 用于 focus pulsing (200ms 切换 BOLD/normal), 压缩状态边框 (500ms 红色脉冲) 等。
///
/// ## 用法示例
/// ```ignore
/// use abacus_ui_kit::hooks::PulseGate;
///
/// if PulseGate::is_on(200) {
///     // BOLD modifier
/// }
/// ```
pub struct PulseGate;

impl PulseGate {
    /// 当前是否处于 on 半周期（true = on, false = off）
    ///
    /// ## 参数
    /// - `half_period_ms`: 半周期毫秒数. on/off 各占此时长
    ///   - 200 → BOLD 200ms / normal 200ms / 总周期 400ms
    pub fn is_on(half_period_ms: u64) -> bool {
        Self::is_on_at(now_subsec_millis(), half_period_ms)
    }

    /// 测试友好版本
    pub fn is_on_at(now_ms: u64, half_period_ms: u64) -> bool {
        let p = half_period_ms.max(1);
        (now_ms / p) % 2 == 0
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ShimmerPhase —— 光带 phase 计算
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 光带 phase 计算器 —— 用于消息卡顶部 streaming shimmer
///
/// ## 算法
///
/// 1. 输入 frame tick（每帧 +1）和 frame interval（通常 50ms）
/// 2. 换算为绝对 ms: tick * frame_ms
/// 3. 按 period_ms 取模得到当前周期内位置 [0, period_ms)
/// 4. 线性映射到 [0, span) 区间, span = bar_len + visible_width
/// 5. 减去 bar_len 得到 phase: 范围 [-bar_len, visible_width - 1]
///
/// 负值表示光带"还没完全进入可见区"; 正值表示光带头部已露出。
///
/// ## 用法示例
/// ```ignore
/// use abacus_ui_kit::hooks::ShimmerPhase;
///
/// let phase = ShimmerPhase::compute(
///     anim_tick,
///     50,    // frame_ms (主循环 50ms 一帧)
///     3500,  // period_ms (3.5 秒一轮)
///     8,     // bar_len (光带 8 cell)
///     inner_width,
/// );
/// ```
pub struct ShimmerPhase;

impl ShimmerPhase {
    /// 计算光带头部当前在可见区的 x 坐标
    pub fn compute(
        anim_tick: u64,
        frame_ms: u64,
        period_ms: u64,
        bar_len: u16,
        visible_width: u16,
    ) -> i32 {
        let now_ms = anim_tick.saturating_mul(frame_ms);
        let p = period_ms.max(1);
        let progress = (now_ms % p) as f64 / p as f64; // [0.0, 1.0)
        let span = (visible_width as u64 + bar_len as u64).max(1);
        ((progress * span as f64) as i64 - bar_len as i64) as i32
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 内部 helper
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 当前时间的 subsec 毫秒数（[0, 1000)）—— 用于 wall-clock 节拍
///
/// 故意只取 subsec 而非完整 epoch ms, 避免 spinner 重启后从奇怪相位开始
fn now_subsec_millis() -> u64 {
    let total = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    // 用 millis 全量以保证 PulseGate 周期 > 1s 也能工作
    total.as_millis() as u64
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 测试
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_frames_cycle() {
        // 10 帧 × 100ms = 1000ms 完整周期
        for i in 0..10 {
            let ms = i * 100;
            let frame = SpinnerFrame::current_at(ms, 100);
            assert_eq!(frame, SPINNER_FRAMES[i as usize]);
        }
        // 1000ms 后回到第 0 帧
        assert_eq!(SpinnerFrame::current_at(1000, 100), SPINNER_FRAMES[0]);
    }

    #[test]
    fn spinner_handles_zero_interval() {
        // interval_ms=0 不应 panic
        let _ = SpinnerFrame::current_at(500, 0);
    }

    #[test]
    fn pulse_gate_alternates() {
        // half=200: [0, 200) on, [200, 400) off, [400, 600) on...
        assert!(PulseGate::is_on_at(0, 200));
        assert!(PulseGate::is_on_at(199, 200));
        assert!(!PulseGate::is_on_at(200, 200));
        assert!(!PulseGate::is_on_at(399, 200));
        assert!(PulseGate::is_on_at(400, 200));
    }

    #[test]
    fn shimmer_phase_ranges_correctly() {
        // bar=8, visible=20, span=28, period=3500ms, frame=50ms
        // tick=0 → progress=0 → phase = 0 - 8 = -8
        let phase = ShimmerPhase::compute(0, 50, 3500, 8, 20);
        assert_eq!(phase, -8);

        // tick=70 → ms=3500 → progress=0 (整周期回到起点) → phase = -8
        let phase = ShimmerPhase::compute(70, 50, 3500, 8, 20);
        assert_eq!(phase, -8);

        // tick=35 → ms=1750 → progress=0.5 → phase = (0.5 * 28) - 8 = 14 - 8 = 6
        let phase = ShimmerPhase::compute(35, 50, 3500, 8, 20);
        assert_eq!(phase, 6);
    }

    #[test]
    fn shimmer_phase_handles_zero_period() {
        // period_ms=0 应被 clamp 到 1, 不 panic
        let _ = ShimmerPhase::compute(100, 50, 0, 8, 20);
    }
}
