//! IDE 视觉效果 — Diff 渲染 + 逐行 Flash 动效
//!
//! 1. Diff 渲染：检测 +/- 行前缀，应用绿色/红色标记
//! 2. Line Flash：新输出的行在 300ms 内显示高亮背景，之后渐退
//!
//! 引用关系：被 components/mod.rs 的消息渲染调用
//! 生命周期：flash state 在 AppState 中维护，每帧检查过期

use std::time::Instant;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Diff 行类型
#[derive(Clone, Copy, PartialEq)]
pub enum DiffType {
    Added,      // + 行（绿色）
    Removed,    // - 行（红色）
    Context,    // 普通上下文行
    Header,     // @@ 行（蓝色）
}

/// 检测一行文本是否为 diff 格式并返回类型
pub fn detect_diff_line(line: &str) -> DiffType {
    if line.starts_with('+') && !line.starts_with("+++") {
        DiffType::Added
    } else if line.starts_with('-') && !line.starts_with("---") {
        DiffType::Removed
    } else if line.starts_with("@@") {
        DiffType::Header
    } else {
        DiffType::Context
    }
}

/// 为 diff 行应用颜色样式
pub fn diff_style(diff_type: DiffType, theme_success: Color, theme_error: Color, theme_accent: Color) -> Style {
    match diff_type {
        DiffType::Added => Style::default().fg(theme_success),
        DiffType::Removed => Style::default().fg(theme_error),
        DiffType::Header => Style::default().fg(theme_accent).add_modifier(Modifier::BOLD),
        DiffType::Context => Style::default(),
    }
}

/// 检测代码块内容是否为 diff 格式（F5 完善：避免 TypeScript ++i / Haskell + - 函数误判）
pub fn is_diff_content(code: &str) -> bool {
    let lines: Vec<&str> = code.lines().collect();
    if lines.is_empty() { return false; }

    // 强信号 1：含 hunk 头 @@ ... @@
    if lines.iter().any(|l| l.starts_with("@@") && l.matches("@@").count() >= 2) {
        return true;
    }
    // 强信号 2：含 unified diff 文件头 +++ / --- (后跟空格或路径)
    if lines.iter().any(|l| l.starts_with("+++ ") || l.starts_with("--- ")) {
        return true;
    }
    // 弱信号：非空行 ≥4 且 +/- 标记占比 ≥50%（且跳过 ++/-- 连写）
    let non_empty: Vec<&str> = lines.iter().copied().filter(|l| !l.trim().is_empty()).collect();
    if non_empty.len() < 4 { return false; }
    let diff_count = non_empty.iter().filter(|l| {
        let bytes = l.as_bytes();
        if bytes.len() < 2 { return false; }
        (bytes[0] == b'+' && bytes[1] != b'+')
            || (bytes[0] == b'-' && bytes[1] != b'-')
    }).count();
    diff_count >= 4 && diff_count * 2 >= non_empty.len()
}

// ══════════════════════════════════════════════════════════════
// Line Flash — 新行高亮渐退效果
// ══════════════════════════════════════════════════════════════

/// Flash 状态：记录哪些行需要高亮
#[derive(Clone)]
pub struct FlashState {
    /// (行内容hash, 创建时间) — 用于检测哪些行是"新的"
    entries: Vec<(u64, Instant)>,
    /// flash 持续时间（默认 300ms）
    pub duration_ms: u64,
}

impl Default for FlashState {
    fn default() -> Self {
        Self::new()
    }
}

impl FlashState {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            duration_ms: 300,
        }
    }

    /// K6 完善：按行内容计算 hash 存入；渲染层可按 hash 精确匹配（不再按底部偏移漂移）
    pub fn mark_new_lines(&mut self, line_contents: &[&str]) {
        let now = Instant::now();
        for content in line_contents {
            self.entries.push((Self::hash_line(content), now));
        }
        // 限制大小
        if self.entries.len() > 200 {
            self.entries.drain(..self.entries.len() - 200);
        }
    }

    /// 当前 flash 窗口内的有效行数（用于状态指示，不依赖位置）
    pub fn active_flash_count(&self) -> usize {
        let now = Instant::now();
        let duration = std::time::Duration::from_millis(self.duration_ms);
        self.entries.iter()
            .filter(|(_, t)| now.duration_since(*t) < duration)
            .count()
    }

    /// 清理过期 entries
    pub fn cleanup(&mut self) {
        let now = Instant::now();
        let duration = std::time::Duration::from_millis(self.duration_ms);
        self.entries.retain(|(_, t)| now.duration_since(*t) < duration);
    }

    /// K6 完善：按内容 hash 判定是否在 flash 窗口内（替代旧的“底部偏移”漂移逻辑）
    pub fn is_flashing(&self, line_hash: u64) -> bool {
        let now = Instant::now();
        let duration = std::time::Duration::from_millis(self.duration_ms);
        self.entries.iter().any(|(h, t)| {
            *h == line_hash && now.duration_since(*t) < duration
        })
    }

    /// 计算单行 hash（供调用方使用，与 mark_new_lines 一致的算法）
    pub fn hash_line(content: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    }
}

/// 给一行应用 flash 高亮（accent 背景色）
pub fn apply_flash_style(line: Line<'static>, flash_color: Color) -> Line<'static> {
    let spans: Vec<Span<'static>> = line.spans.into_iter().map(|mut span| {
        let mut style = span.style;
        style = style.bg(flash_color);
        span.style = style;
        span
    }).collect();
    Line::from(spans)
}

// ═════════════════════════════════════════════════════════════
// Auto Contrast — 按背景色亮度自动选择前景色 (K3d 依赖)
// ═════════════════════════════════════════════════════════════

/// 按背景色返回高对比度的前景色。
///
/// 算法：WCAG 简化亮度公式 L = 0.299*R + 0.587*G + 0.114*B；
/// 阈值 128 划分深/浅；返回近黑 (20,20,20) 或近白 (245,245,245)。
/// 非 Rgb 类型 (Reset/Indexed/名称色) 返回 Color::Reset，交由终端默认。
///
/// 引用关系：被 components/mod.rs 的按钮/chip 渲染调用 (K3d)
/// 生命周期：纯函数、无状态
pub fn auto_contrast_fg(bg: Color) -> Color {
    match bg {
        Color::Rgb(r, g, b) => {
            // 加权亮度（不用 sRGB linearize，终端场景下近似足够）
            let luminance = (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000;
            if luminance > 128 {
                Color::Rgb(20, 20, 20)
            } else {
                Color::Rgb(245, 245, 245)
            }
        }
        // 名称色：粗略映射
        Color::Black | Color::DarkGray | Color::Blue | Color::Red
        | Color::Green | Color::Magenta | Color::Cyan => Color::Rgb(245, 245, 245),
        Color::White | Color::Gray | Color::LightYellow => Color::Rgb(20, 20, 20),
        // Indexed / Reset / 其它：交给终端
        _ => Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_contrast_dark_bg_returns_light() {
        let fg = auto_contrast_fg(Color::Rgb(20, 20, 25));
        assert!(matches!(fg, Color::Rgb(245, 245, 245)));
    }

    #[test]
    fn auto_contrast_light_bg_returns_dark() {
        let fg = auto_contrast_fg(Color::Rgb(240, 240, 245));
        assert!(matches!(fg, Color::Rgb(20, 20, 20)));
    }

    #[test]
    fn auto_contrast_mid_gray_picks_one() {
        // (128, 128, 128) 亮度 ≈ 128，临界取何一都可接受
        let fg = auto_contrast_fg(Color::Rgb(128, 128, 128));
        assert!(matches!(fg, Color::Rgb(_, _, _)));
    }

    #[test]
    fn auto_contrast_reset_returns_reset() {
        let fg = auto_contrast_fg(Color::Reset);
        assert!(matches!(fg, Color::Reset));
    }
}
