//! ScrollableStack —— 消息流滚动位置 + 命中测试
//!
//! ## 设计目标
//!
//! 替代 V40 的 `cached_msg_rows` + `message_trace_row_map` 双缓存。
//! 维护一个**单次渲染**的 `item_areas` 快照 —— 每张卡片映射到屏幕区域。
//! hit-test 反查 O(1), 滚动边界处理内聚到一处。
//!
//! ## 不变量
//!
//! 1. `item_areas` 与 `CardStream` 严格同步 (每次 render 时重建)
//! 2. `viewport_top` 表示"最顶部可见卡片"在 item_areas 中的索引
//! 3. 滚动边界 (上/下) 不允许越界 viewport
//!
//! ## 性能
//!
//! - `item_areas` 是 Vec<(id, Rect)>, 单次 push O(1)
//! - 重建一次 O(n) (n = 卡片数, 通常 < 1000)
//! - hit-test 二分 O(log n), 不需要全表扫描

use ratatui::layout::Rect;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ScrollLayout —— 单次渲染的卡片区域快照
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 单次渲染的卡片区域快照 —— 由 `rebuild()` 写入
///
/// 用途:
/// - hit-test: 给定 (x, y) 反查命中哪张卡片 (二分查找)
/// - 滚动边界: 已知每张卡片高度, 累加求 scroll_to_bottom
/// - 调试: 鼠标 hover 高亮时, 给定 index 知道精确 Rect
#[derive(Debug, Clone, Default)]
pub struct ScrollLayout {
    /// (card_id, body_rect) — 按 cards 顺序排列
    /// body_rect 是卡片在屏幕上的完整区域 (含边框 + header)
    /// 不含 bottom padding
    pub items: Vec<(u64, Rect)>,
    /// 累计高度 (sum of items[].height)
    pub total_height: u16,
    /// viewport 高度 (即 messages 区域的高度)
    pub viewport_height: u16,
}

impl ScrollLayout {
    pub fn new() -> Self {
        Self::default()
    }

    /// 清空 (新一次渲染开始)
    pub fn clear(&mut self) {
        self.items.clear();
        self.total_height = 0;
    }

    /// 推入一张卡片 (render 时按顺序调用)
    pub fn push(&mut self, id: u64, rect: Rect) {
        self.items.push((id, rect));
        self.total_height = self.total_height.saturating_add(rect.height);
    }

    /// hit-test: 给定屏幕坐标 (x, y) 反查 (card_id, 内部相对 y)
    /// 返回 (id, 卡片顶部 y 偏移, 卡片高度)
    /// None = 不在任何卡片内
    ///
    /// 内部使用二分: items 按 y 排序 (rebuild 时按 push 顺序保证), O(log n)
    pub fn hit_test(&self, x: u16, y: i32) -> Option<(u64, i32)> {
        if self.items.is_empty() {
            return None;
        }
        // 二分找最右侧 rect.y <= y 的项
        let mut lo = 0usize;
        let mut hi = self.items.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if (self.items[mid].1.y as i32) <= y {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // lo - 1 是候选项
        if lo == 0 {
            return None;
        }
        let (id, rect) = self.items[lo - 1];
        let rect_bottom = rect.y as i32 + rect.height as i32;
        if y >= rect_bottom {
            return None; // 在卡片下方空白
        }
        if x < rect.x || x >= rect.x.saturating_add(rect.width) {
            return None; // x 范围外
        }
        Some((id, y - rect.y as i32))
    }

    /// 给定 card_id 查其 area
    pub fn area_of(&self, id: u64) -> Option<Rect> {
        self.items.iter().find(|(cid, _)| *cid == id).map(|(_, r)| *r)
    }

    /// 给定 card_id 查其索引
    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.items.iter().position(|(cid, _)| *cid == id)
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ScrollPosition —— 滚动位置状态
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 消息流滚动位置 —— 独立于 layout 快照的持久状态
///
/// ## 模式
///
/// - `Top` (0): 滚到顶部 (最早消息)
/// - `Bottom` (u16::MAX): 粘附底部 (新消息追加时自动跟随)
/// - 其他: 像素偏移, 从 top 算起
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollPosition(pub u16);

impl ScrollPosition {
    /// 粘附底部 —— 等同 `u16::MAX`
    pub const BOTTOM: Self = Self(u16::MAX);

    pub fn new(offset: u16) -> Self {
        Self(offset)
    }

    pub fn is_bottom(self) -> bool {
        self.0 == u16::MAX
    }

    pub fn offset(self) -> u16 {
        if self.is_bottom() { 0 } else { self.0 }
    }
}

impl Default for ScrollPosition {
    fn default() -> Self {
        Self::BOTTOM
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ScrollableStack —— 滚动状态 + 边界约束
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 消息流滚动栈 —— 封装"渲染"和"滚动"两套独立关注点
///
/// ## 用法
///
/// ```ignore
/// // 渲染时:
/// let mut layout = ScrollLayout::new();
/// for card in stream.iter() { layout.push(card.id(), rect); }
///
/// // 滚动处理时:
/// stack.clamp_to_bottom(&layout);  // BOTTOM 模式自动跟随
/// stack.scroll_up(3, &layout);
/// stack.scroll_down(3, &layout);
/// ```
#[derive(Debug, Clone)]
pub struct ScrollableStack {
    /// 当前滚动位置
    pub position: ScrollPosition,
    /// viewport 高度 (缓存, rebuild 时同步)
    pub viewport_height: u16,
}

impl Default for ScrollableStack {
    fn default() -> Self {
        Self {
            position: ScrollPosition::BOTTOM,
            viewport_height: 0,
        }
    }
}

impl ScrollableStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// 重建滚动状态 (每次 render_messages 调用前)
    ///
    /// - 如果 position 是 BOTTOM 模式: 保持 BOTTOM (不修改, 由 render 端计算)
    /// - 如果 position 越界 (> total_height - viewport): clamp 到 max_offset
    pub fn rebuild(&mut self, layout: &ScrollLayout) {
        self.viewport_height = layout.viewport_height;
        if self.position.is_bottom() {
            return; // 粘附模式不修改
        }
        let max_offset = layout
            .total_height
            .saturating_sub(layout.viewport_height);
        if self.position.0 > max_offset {
            self.position.0 = max_offset;
        }
    }

    /// 滚到顶部
    pub fn scroll_to_top(&mut self) {
        self.position = ScrollPosition::new(0);
    }

    /// 滚到底部 (粘附模式)
    pub fn scroll_to_bottom(&mut self) {
        self.position = ScrollPosition::BOTTOM;
    }

    /// 向上滚 n 行
    pub fn scroll_up(&mut self, n: u16) {
        let cur = self.position.offset();
        self.position = ScrollPosition::new(cur.saturating_sub(n));
    }

    /// 向下滚 n 行
    pub fn scroll_down(&mut self, n: u16, layout: &ScrollLayout) {
        let cur = self.position.offset();
        let max = layout
            .total_height
            .saturating_sub(layout.viewport_height);
        // 向下到 max 即视作"到底", 切回 BOTTOM 粘附模式
        let next = cur.saturating_add(n).min(max);
        if next >= max {
            self.position = ScrollPosition::BOTTOM;
        } else {
            self.position = ScrollPosition::new(next);
        }
    }

    /// 切换"粘附底部" (按 G / End)
    pub fn toggle_stick_bottom(&mut self) {
        if self.position.is_bottom() {
            self.position = ScrollPosition::new(0);
        } else {
            self.position = ScrollPosition::BOTTOM;
        }
    }

    /// 是否有新消息追加 (用于决定是否切回 BOTTOM 粘附)
    /// 旧 last_id → 新 last_id, 若 last_id 变了 + position=BOTTOM 不变
    /// 调用方决定是否调用
    pub fn is_at_bottom(&self) -> bool {
        self.position.is_bottom()
    }
}

use crate::scrollable::Scrollable;

impl Scrollable for ScrollableStack {
    fn scroll_up(&mut self, n: usize) {
        ScrollableStack::scroll_up(self, n as u16);
    }

    fn scroll_down(&mut self, n: usize, content_len: usize) {
        let layout = ScrollLayout {
            items: Vec::new(),
            total_height: content_len as u16,
            viewport_height: self.viewport_height,
        };
        ScrollableStack::scroll_down(self, n as u16, &layout);
    }

    fn scroll_to_top(&mut self) {
        ScrollableStack::scroll_to_top(self);
    }

    fn scroll_to_bottom(&mut self, _content_len: usize) {
        ScrollableStack::scroll_to_bottom(self);
    }

    fn offset(&self) -> usize {
        self.position.offset() as usize
    }

    fn is_at_bottom(&self) -> bool {
        ScrollableStack::is_at_bottom(self)
    }

    fn clamp(&mut self, content_len: usize, viewport_height: usize) {
        let layout = ScrollLayout {
            items: Vec::new(),
            total_height: content_len as u16,
            viewport_height: viewport_height as u16,
        };
        ScrollableStack::rebuild(self, &layout);
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 测试
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::kinds;
    use crate::card::{CardStreaming, MessageCard};
    use crate::card_stream::CardStream;
    use crate::section::SectionContext;
    use crate::Theme;
    use ratatui::Frame;

    #[allow(dead_code)]
    struct TestCtx(Theme);
    #[allow(dead_code)]
    impl SectionContext for TestCtx {
        fn theme(&self) -> &Theme { &self.0 }
    }

    struct TinyCard(u64, #[allow(dead_code)] u16);
    impl MessageCard for TinyCard {
        fn kind(&self) -> &'static str { kinds::USER }
        fn id(&self) -> u64 { self.0 }
        fn header(&self, _: &dyn SectionContext) -> crate::card::CardHeader {
            crate::card::CardHeader::new("t", "0")
        }
        fn streaming(&self) -> CardStreaming { CardStreaming::Static }
        fn body_height(&self, _: &dyn SectionContext, _: u16, _: crate::card::CardCollapse) -> u16 {
            1 // 占位
        }
        fn render_body(&self, _: &mut Frame, _: &dyn SectionContext, _: Rect, _: crate::card::CardCollapse) {}
    }

    fn make_layout(heights: &[u16], viewport: u16, y0: u16) -> ScrollLayout {
        let mut layout = ScrollLayout::new();
        layout.viewport_height = viewport;
        let mut y = y0;
        for (i, h) in heights.iter().enumerate() {
            let rect = Rect::new(0, y, 80, *h);
            layout.push(i as u64 + 1, rect);
            y += h;
        }
        layout
    }

    #[test]
    fn layout_push_accumulates_total_height() {
        let mut l = ScrollLayout::new();
        l.push(1, Rect::new(0, 0, 80, 5));
        l.push(2, Rect::new(0, 5, 80, 3));
        l.push(3, Rect::new(0, 8, 80, 7));
        assert_eq!(l.total_height, 15);
        assert_eq!(l.items.len(), 3);
    }

    #[test]
    fn layout_clear_resets_state() {
        let mut l = make_layout(&[5, 3, 7], 20, 0);
        assert!(!l.items.is_empty());
        l.clear();
        assert!(l.items.is_empty());
        assert_eq!(l.total_height, 0);
    }

    #[test]
    fn layout_hit_test_finds_correct_card() {
        let l = make_layout(&[5, 3, 7], 20, 0);
        // (10, 0) 在卡片 1 (y=0..5)
        assert_eq!(l.hit_test(10, 0), Some((1, 0)));
        // (10, 4) 仍在卡片 1
        assert_eq!(l.hit_test(10, 4), Some((1, 4)));
        // (10, 5) 在卡片 2 (y=5..8)
        assert_eq!(l.hit_test(10, 5), Some((2, 0)));
        // (10, 7) 仍在卡片 2
        assert_eq!(l.hit_test(10, 7), Some((2, 2)));
        // (10, 8) 在卡片 3 (y=8..15)
        assert_eq!(l.hit_test(10, 8), Some((3, 0)));
        // (10, 14) 仍在卡片 3
        assert_eq!(l.hit_test(10, 14), Some((3, 6)));
    }

    #[test]
    fn layout_hit_test_outside_returns_none() {
        let l = make_layout(&[5, 3, 7], 20, 0);
        // y < 0
        assert_eq!(l.hit_test(10, -1), None);
        // y 超过最后卡片
        assert_eq!(l.hit_test(10, 20), None);
        // x 越界 (card 1 width=80, x=0..80)
        assert_eq!(l.hit_test(200, 2), None);
    }

    #[test]
    fn layout_hit_test_empty_returns_none() {
        let l = ScrollLayout::new();
        assert_eq!(l.hit_test(0, 0), None);
    }

    #[test]
    fn layout_area_of_finds_by_id() {
        let l = make_layout(&[5, 3, 7], 20, 0);
        assert_eq!(l.area_of(1), Some(Rect::new(0, 0, 80, 5)));
        assert_eq!(l.area_of(2), Some(Rect::new(0, 5, 80, 3)));
        assert_eq!(l.area_of(3), Some(Rect::new(0, 8, 80, 7)));
        assert_eq!(l.area_of(99), None);
    }

    #[test]
    fn layout_index_of_finds_position() {
        let l = make_layout(&[5, 3, 7], 20, 0);
        assert_eq!(l.index_of(1), Some(0));
        assert_eq!(l.index_of(2), Some(1));
        assert_eq!(l.index_of(3), Some(2));
        assert_eq!(l.index_of(99), None);
    }

    #[test]
    fn scroll_position_default_is_bottom() {
        let p = ScrollPosition::default();
        assert!(p.is_bottom());
    }

    #[test]
    fn scroll_position_offset_converts_bottom_to_zero() {
        let p = ScrollPosition::BOTTOM;
        assert_eq!(p.offset(), 0);
        let p2 = ScrollPosition::new(42);
        assert_eq!(p2.offset(), 42);
    }

    #[test]
    fn scroll_stack_rebuild_clamps_overflow() {
        let mut s = ScrollableStack::new();
        let l = make_layout(&[5, 3, 7], 20, 0); // total=15, viewport=20
        // total < viewport: max_offset=0
        s.position = ScrollPosition::new(100); // 越界
        s.rebuild(&l);
        assert_eq!(s.position, ScrollPosition::new(0));
    }

    #[test]
    fn scroll_stack_rebuild_keeps_bottom() {
        let mut s = ScrollableStack::new();
        let l = make_layout(&[5, 3, 7], 20, 0);
        assert!(s.position.is_bottom());
        s.rebuild(&l);
        assert!(s.position.is_bottom());
    }

    #[test]
    fn scroll_stack_scroll_up_down_basic() {
        let mut s = ScrollableStack::new();
        s.position = ScrollPosition::BOTTOM; // 当前 BOTTOM
        // 切到顶部才能 scroll
        s.scroll_to_top();
        assert_eq!(s.position, ScrollPosition::new(0));
        // 向上滚 3 (顶部不能再上)
        s.scroll_up(3);
        assert_eq!(s.position, ScrollPosition::new(0));
    }

    #[test]
    fn scroll_stack_scroll_down_reaches_bottom() {
        let mut s = ScrollableStack::new();
        let l = make_layout(&[10, 10, 10, 10], 20, 0); // total=40, viewport=20, max=20
        s.position = ScrollPosition::new(0);
        s.scroll_down(100, &l); // 一次滚到底
        assert!(s.position.is_bottom());
    }

    #[test]
    fn scroll_stack_scroll_down_partial() {
        let mut s = ScrollableStack::new();
        let l = make_layout(&[10, 10, 10, 10], 20, 0);
        s.position = ScrollPosition::new(0);
        s.scroll_down(5, &l);
        assert_eq!(s.position, ScrollPosition::new(5));
    }

    #[test]
    fn scroll_stack_toggle_stick_bottom() {
        let mut s = ScrollableStack::new();
        assert!(s.is_at_bottom());
        s.toggle_stick_bottom();
        assert!(!s.is_at_bottom());
        s.toggle_stick_bottom();
        assert!(s.is_at_bottom());
    }

    #[test]
    fn card_stream_last_id_returns_last_pushed() {
        let mut s = CardStream::new();
        assert!(s.last_id().is_none());
        let a = s.alloc_id();
        s.push_static(Box::new(TinyCard(a, 1)));
        let b = s.alloc_id();
        s.push_static(Box::new(TinyCard(b, 1)));
        assert_eq!(s.last_id(), Some(b));
    }
}
