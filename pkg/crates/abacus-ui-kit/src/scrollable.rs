//! Scrollable — 统一滚动容器 trait
//!
//! ## 设计目标
//!
//! 抽象 4 类滚动容器的公共接口，消除 event/mod.rs 和 overlays.rs 中的重复逻辑。
//!
//! ## 实现者
//!
//! - `SimpleScrollOffset` — 面板区域（timeline/knowledge）、编辑器、命令提示（usize 偏移）
//! - `ScrollableStack` — 消息流（像素级 + BOTTOM 粘附）

/// 统一滚动容器 trait
///
/// 所有滚动容器共享的操作接口。
/// `content_len` 参数表示内容总行数（或总高度），由调用方在滚动时传入。
pub trait Scrollable {
    /// 向上滚动 n 行（远离最新内容）
    fn scroll_up(&mut self, n: usize);

    /// 向下滚动 n 行（接近最新内容）
    fn scroll_down(&mut self, n: usize, content_len: usize);

    /// 滚到顶部
    fn scroll_to_top(&mut self);

    /// 滚到底部
    fn scroll_to_bottom(&mut self, content_len: usize);

    /// 当前偏移（从顶部算起的行数/像素）
    fn offset(&self) -> usize;

    /// 是否在底部
    fn is_at_bottom(&self) -> bool;

    /// 将偏移 clamp 到合法范围 [0, content_len - viewport_height]
    fn clamp(&mut self, content_len: usize, viewport_height: usize);
}

/// 简单偏移滚动 —— 用于面板区域、编辑器等 offset-from-top 的容器
///
/// 内部维护一个 `usize` 偏移量（从顶部算起的行数）。
/// 0 = 在顶部；`content_len - viewport_height` = 在底部。
#[derive(Debug, Clone, Default)]
pub struct SimpleScrollOffset {
    pub offset: usize,
    /// 是否跟随最新内容（类似 ScrollableStack 的 BOTTOM 模式）
    pub follow_tail: bool,
}

impl SimpleScrollOffset {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_follow_tail(follow: bool) -> Self {
        Self {
            offset: 0,
            follow_tail: follow,
        }
    }
}

impl Scrollable for SimpleScrollOffset {
    fn scroll_up(&mut self, n: usize) {
        self.follow_tail = false;
        self.offset = self.offset.saturating_sub(n);
    }

    fn scroll_down(&mut self, n: usize, content_len: usize) {
        let max = content_len.saturating_sub(1);
        self.offset = self.offset.saturating_add(n).min(max);
        if self.offset >= max {
            self.follow_tail = true;
        }
    }

    fn scroll_to_top(&mut self) {
        self.follow_tail = false;
        self.offset = 0;
    }

    fn scroll_to_bottom(&mut self, content_len: usize) {
        self.follow_tail = true;
        self.offset = content_len.saturating_sub(1);
    }

    fn offset(&self) -> usize {
        self.offset
    }

    fn is_at_bottom(&self) -> bool {
        self.follow_tail
    }

    fn clamp(&mut self, content_len: usize, viewport_height: usize) {
        if self.follow_tail {
            return;
        }
        let max = content_len.saturating_sub(viewport_height);
        if self.offset > max {
            self.offset = max;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_scroll_offset_default() {
        let s = SimpleScrollOffset::new();
        assert_eq!(s.offset(), 0);
        assert!(!s.is_at_bottom());
    }

    #[test]
    fn simple_scroll_up_down() {
        let mut s = SimpleScrollOffset::new();
        s.scroll_down(5, 100);
        assert_eq!(s.offset(), 5);
        assert!(!s.is_at_bottom());

        s.scroll_up(2);
        assert_eq!(s.offset(), 3);
    }

    #[test]
    fn simple_scroll_up_saturates_at_zero() {
        let mut s = SimpleScrollOffset::new();
        s.scroll_up(10);
        assert_eq!(s.offset(), 0);
    }

    #[test]
    fn simple_scroll_down_to_bottom_sets_follow_tail() {
        let mut s = SimpleScrollOffset::new();
        s.scroll_down(100, 100);
        assert_eq!(s.offset(), 99);
        assert!(s.is_at_bottom());
    }

    #[test]
    fn simple_scroll_to_top_resets_follow() {
        let mut s = SimpleScrollOffset::with_follow_tail(true);
        s.scroll_to_top();
        assert_eq!(s.offset(), 0);
        assert!(!s.is_at_bottom());
    }

    #[test]
    fn simple_scroll_to_bottom() {
        let mut s = SimpleScrollOffset::new();
        s.scroll_to_bottom(50);
        assert_eq!(s.offset(), 49);
        assert!(s.is_at_bottom());
    }

    #[test]
    fn simple_clamp_limits_offset() {
        let mut s = SimpleScrollOffset::new();
        s.offset = 200;
        s.clamp(100, 20);
        assert_eq!(s.offset(), 80);
    }

    #[test]
    fn simple_clamp_follow_tail_skips() {
        let mut s = SimpleScrollOffset::with_follow_tail(true);
        s.offset = 999;
        s.clamp(100, 20);
        assert_eq!(s.offset(), 999); // follow_tail skips clamp
    }
}
