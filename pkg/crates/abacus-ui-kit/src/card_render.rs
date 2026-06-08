//! card_render —— 通用卡片渲染 + 流式 shimmer 特效
//!
//! ## 设计目标
//!
//! 把 `render_messages_in_card` 里 1439 行的"按角色分支"逻辑提取到一处
//! 通用 render 函数。每张 Card 只需实现 `body_height` + `render_body`,
//! 边框/header/折叠/shimmer 由本模块统一处理。
//!
//! ## 视觉契约
//!
//! 边框形态:
//! - `Static`   → BorderType::Rounded `╭─╮╰─╯`
//! - `Active`   → BorderType::Double  `╔═╗╚═╝` + 顶部 shimmer
//! - `Aborted`  → BorderType::Plain   `┌─┐└─┘` + 边框 error 色
//!
//! Header 行:
//! ```text
//! ╭─ {title} ··· {trailing} ──╮
//! ```
//! 折叠箭头:
//! - Expanded  → `▾` 底部右侧
//! - Collapsed → `▸` 底部右侧
//! - Headless  → 不画 body
//!
//! 续行符 `┊` 用于长行 wrap 后的视觉延续 (V40 沿用, 见 block_detail 5 路径后缀)
//!
//! ## 与 V40 的对照
//!
//! - V40: messages.rs 1439 行, 包含 4 种角色分支 + cache + dirty + geometry
//! - V42-B: render_card() 一处, 4 种 Card 各自实现 body, 共享 header/border/shimmer

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};

use crate::card::{default_color_for_kind, CardCollapse, CardHeader, CardHit, CardStreaming, MessageCard};
use crate::section::SectionContext;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 卡片整体高度 = 边框(2) + header(1) + body(N)
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 卡片在屏幕上的总高度 (含边框 + header)
pub fn card_total_height(
    card: &dyn MessageCard,
    ctx: &dyn SectionContext,
    width: u16,
    collapse: CardCollapse,
) -> u16 {
    let body_h = if matches!(collapse, CardCollapse::Headless) {
        0
    } else {
        card.body_height(ctx, width.saturating_sub(2), collapse) // 减 2 边框
    };
    body_h.saturating_add(2).saturating_add(1) // +2 边框 +1 header
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// render_card —— 通用渲染入口
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 通用卡片渲染 —— 给定 rect 范围, 画边框 + header + body
///
/// ## 调用方责任
///
/// 1. 调用前已计算 `rect` (由 ScrollLayout 决定位置 + 高度)
/// 2. 调用方负责 `body_inner` 的精确 Rect (header 行 1 行 + 内容 N 行)
/// 3. 不修改 `f` 之外的全局状态
///
/// ## 副作用
///
/// 仅写 `f.buffer_mut()`, 不动 AppState。
pub fn render_card(
    f: &mut Frame,
    card: &dyn MessageCard,
    ctx: &dyn SectionContext,
    rect: Rect,
    collapse: CardCollapse,
    shimmer_pos: i32,
) {
    if rect.width < 3 || rect.height < 3 {
        return; // 太小无法画边框 + header
    }
    let header = card.header(ctx);
    let color = header.color.unwrap_or_else(|| {
        default_color_for_kind(card.kind(), ctx.theme())
    });

    // 边框策略：统一 Rounded（不再切 Double/Plain），border 用弱色
    // 让消息流视觉连续，避免边框加粗造成割裂感
    let border_color = match card.streaming() {
        CardStreaming::Aborted => ctx.theme().error,
        _ => ctx.theme().border,  // 弱色边框，不抢主色
    };

    // 边框 + 标题 + 折叠箭头
    let title_spans = build_title_spans(&header, &color, ctx, shimmer_pos);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(title_spans);

    if matches!(collapse, CardCollapse::Headless) {
        // 仅画 header (1 行 + 2 边框 = 3 行, rect 至少 3 高)
        let p = Paragraph::new("").block(block);
        f.render_widget(p, rect);
        return;
    }

    // 计算 body 区域 (扣 1 行 header + 2 行边框)
    let body_h = card.body_height(ctx, rect.width.saturating_sub(2), collapse);
    let total = 2u16.saturating_add(1).saturating_add(body_h);
    if rect.height < total {
        // 高度不够 — 截断 body 到 (rect.height - 3)
        let truncated_h = rect.height.saturating_sub(3);
        let inner = Rect::new(
            rect.x.saturating_add(1),
            rect.y.saturating_add(2),
            rect.width.saturating_sub(2),
            truncated_h,
        );
        let p = Paragraph::new("")
            .block(block)
            .wrap(Wrap { trim: false });
        f.render_widget(p, rect);
        // 用 inner_area 调 card.render_body 让卡片自己截断
        card.render_body(f, ctx, inner, collapse);
        return;
    }

    let inner = Rect::new(
        rect.x.saturating_add(1),
        rect.y.saturating_add(2),
        rect.width.saturating_sub(2),
        body_h,
    );

    let p = Paragraph::new("")
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(p, rect);
    card.render_body(f, ctx, inner, collapse);

    // 折叠箭头 (右下角) — 仅 Expanded/Collapsed 状态
    let arrow = match collapse {
        CardCollapse::Expanded => "▾",
        CardCollapse::Collapsed => "▸",
        CardCollapse::Headless => return, // 不会到这里
    };
    let arrow_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let arrow_span = Span::styled(arrow, arrow_style);
    let arrow_x = rect.x.saturating_add(rect.width.saturating_sub(2));
    let arrow_y = rect.y.saturating_add(rect.height.saturating_sub(1));
    f.render_widget(
        Paragraph::new(Line::from(arrow_span)),
        Rect::new(arrow_x, arrow_y, 1, 1),
    );
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// paint_card_top_shimmer —— 流式卡片顶部 shimmer 特效
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 在卡片顶部边框画 shimmer 光带 (流式期间调用)
///
/// ## 视觉
///
/// 仅在 Active 状态调用。沿顶部边框 (1 行) 画一条渐变色光带, 颜色 = 卡片主色。
/// 光带位置由 `shimmer_pos` 决定 —— 来自 `ShimmerPhase::compute()`, 单位是 cell 偏移
/// (可能为负, 表示光带尚未进入可见区)。
///
/// ## 实现
///
/// 简单实现: 取 `shimmer_pos` 映射到 0..(width-2) 范围, 画 3 字符高亮 (主色 BOLD),
/// 2 字符次高亮 (主色普通), 其余边框字符保持原色 DIM, 模拟"光带滑过"效果。
pub fn paint_card_top_shimmer(
    f: &mut Frame,
    rect: Rect,
    color: Color,
    shimmer_pos: i32,
) {
    if rect.width < 5 {
        return;
    }
    let inner_w = rect.width.saturating_sub(2) as usize;
    if inner_w == 0 {
        return;
    }
    // shimmer_pos 可能是负数 (光带尚未进入) 或 >= inner_w (已离开)
    // 在 [-bar_len, inner_w + bar_len] 范围内才画
    let pos = shimmer_pos;
    if pos < -3 || pos > inner_w as i32 + 3 {
        return;
    }
    // 在顶部边框 y 上, 从 x+1 开始, 渲染 inner_w 个单元格
    // 仅在 [pos-1, pos+1] 范围画 BOLD 高亮
    let top_y = rect.y;
    let top_x_start = rect.x.saturating_add(1);
    let hot_color = color;
    let cool_color = color;
    for dx in 0..inner_w {
        let dx_i = dx as i32;
        let is_hot = (dx_i - pos).abs() <= 1;
        let is_warm = (dx_i - pos).abs() <= 2;
        let style = if is_hot {
            Style::default().fg(hot_color).add_modifier(Modifier::BOLD)
        } else if is_warm {
            Style::default().fg(cool_color)
        } else {
            Style::default().fg(cool_color).add_modifier(Modifier::DIM)
        };
        let cell_rect = Rect::new(top_x_start + dx as u16, top_y, 1, 1);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("─", style))),
            cell_rect,
        );
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 内部: 构造 header 标题 spans
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 构造 header 行的 title spans —— `{title} ··· {trailing} [▸]`
fn build_title_spans(
    header: &CardHeader,
    color: &Color,
    ctx: &dyn SectionContext,
    shimmer_pos: i32,
) -> Line<'static> {
    let title_style = Style::default().fg(*color).add_modifier(Modifier::BOLD);
    let trailing_style = Style::default().fg(ctx.theme().muted);
    let mut spans = vec![Span::styled(header.title.clone(), title_style)];
    if !header.trailing.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(header.trailing.clone(), trailing_style));
    }
    // 流式期间附加 shimmer 字符 (视觉提示, 实际光带在边框上)
    if shimmer_pos != -999 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("●", Style::default().fg(*color)));
    }
    Line::from(spans)
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// hit_test_card —— hit-test 通用入口
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 通用卡片 hit-test —— 给定屏幕坐标反查 CardHit
///
/// 路由:
/// - 点中 header 行 (rect.y) → CardHit::Header
/// - 点中右上角折叠箭头 (rect 右下 - 1) → CardHit::CollapseToggle
/// - 其他 → 转交 card.hit_test (default: Body { inner_y })
/// - rect 外 → None
pub fn hit_test_card(
    card: &dyn MessageCard,
    rect: Rect,
    x: u16,
    y: i32,
) -> Option<CardHit> {
    if rect.width < 3 || rect.height < 3 {
        return None;
    }
    let ry = rect.y as i32;
    let rx = rect.x as i32;
    let rw = rect.width as i32;
    let rh = rect.height as i32;
    if (x as i32) < rx || (x as i32) >= rx + rw || y < ry || y >= ry + rh {
        return None;
    }
    // 折叠箭头位置: (rect.x + rect.width - 2, rect.y + rect.height - 1)
    let arrow_x = rect.x as i32 + rect.width as i32 - 2;
    let arrow_y = rect.y as i32 + rect.height as i32 - 1;
    if (x as i32) == arrow_x && y == arrow_y {
        return Some(CardHit::CollapseToggle);
    }
    // header 行: rect.y
    if y == ry {
        return Some(CardHit::Header);
    }
    // body 区域
    let inner = Rect::new(
        rect.x.saturating_add(1),
        rect.y.saturating_add(2),
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(3),
    );
    card.hit_test(inner, x, y as u16)
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 测试
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;
    use crate::card::kinds;
    use crate::Theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    struct TestCtx(Theme);
    impl SectionContext for TestCtx {
        fn theme(&self) -> &Theme { &self.0 }
    }

    struct StubCard {
        id: u64,
        kind_val: &'static str,
        body_h: u16,
        streaming: CardStreaming,
    }

    impl StubCard {
        fn new(id: u64, kind: &'static str, body_h: u16, streaming: CardStreaming) -> Self {
            Self { id, kind_val: kind, body_h, streaming }
        }
    }

    impl MessageCard for StubCard {
        fn kind(&self) -> &'static str { self.kind_val }
        fn id(&self) -> u64 { self.id }
        fn header(&self, _: &dyn SectionContext) -> CardHeader {
            CardHeader::new("stub", "1.0s")
        }
        fn streaming(&self) -> CardStreaming { self.streaming }
        fn body_height(&self, _: &dyn SectionContext, _: u16, _: CardCollapse) -> u16 {
            self.body_h
        }
        fn render_body(&self, _: &mut Frame, _: &dyn SectionContext, _: Rect, _: CardCollapse) {}
    }

    fn test_ctx() -> TestCtx { TestCtx(Theme::brand()) }

    #[test]
    fn card_total_height_static() {
        let card = StubCard::new(1, kinds::USER, 5, CardStreaming::Static);
        let ctx = test_ctx();
        // body=5, +1 header, +2 border = 8
        assert_eq!(card_total_height(&card, &ctx, 80, CardCollapse::Expanded), 8);
    }

    #[test]
    fn card_total_height_headless_is_3() {
        let card = StubCard::new(1, kinds::USER, 5, CardStreaming::Static);
        let ctx = test_ctx();
        // headless: body=0, +1 header, +2 border = 3
        assert_eq!(card_total_height(&card, &ctx, 80, CardCollapse::Headless), 3);
    }

    #[test]
    fn card_total_height_clamps() {
        // body=u16::MAX - 2 + 3 → 应饱和到 u16::MAX
        let card = StubCard::new(1, kinds::USER, u16::MAX - 2, CardStreaming::Static);
        let ctx = test_ctx();
        let h = card_total_height(&card, &ctx, 80, CardCollapse::Expanded);
        assert_eq!(h, u16::MAX);
    }

    #[test]
    fn render_card_does_not_panic_on_tiny_rect() {
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let card = StubCard::new(1, kinds::USER, 5, CardStreaming::Static);
        let ctx = test_ctx();
        terminal.draw(|f| {
            render_card(f, &card, &ctx, Rect::new(0, 0, 2, 2), CardCollapse::Expanded, -999);
        });
    }

    #[test]
    fn render_card_static_active_aborted_all_render() {
        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let ctx = test_ctx();
        for streaming in [CardStreaming::Static, CardStreaming::Active, CardStreaming::Aborted] {
            let card = StubCard::new(1, kinds::LLM, 3, streaming);
            terminal.draw(|f| {
                render_card(f, &card, &ctx, Rect::new(0, 0, 20, 10), CardCollapse::Expanded, 5);
            });
        }
    }

    #[test]
    fn render_card_headless_draws_only_border_header() {
        let backend = TestBackend::new(20, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let card = StubCard::new(1, kinds::USER, 5, CardStreaming::Static);
        let ctx = test_ctx();
        terminal.draw(|f| {
            render_card(f, &card, &ctx, Rect::new(0, 0, 20, 5), CardCollapse::Headless, -999);
        });
    }

    #[test]
    fn render_card_too_short_rect_truncates_body() {
        // rect 高度 < 3+body_h → 截断 body 到 (rect.height - 3)
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let card = StubCard::new(1, kinds::USER, 10, CardStreaming::Static);
        let ctx = test_ctx();
        terminal.draw(|f| {
            // rect=6 行, body_h=10, 应截断到 6-3=3 行
            render_card(f, &card, &ctx, Rect::new(0, 0, 20, 6), CardCollapse::Expanded, -999);
        });
    }

    #[test]
    fn paint_card_top_shimmer_does_not_panic() {
        let backend = TestBackend::new(30, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| {
            paint_card_top_shimmer(f, Rect::new(0, 0, 30, 3), Color::Magenta, 10);
        });
    }

    #[test]
    fn paint_card_top_shimmer_handles_narrow() {
        let backend = TestBackend::new(4, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| {
            // width < 5 → 直接 return, 不画
            paint_card_top_shimmer(f, Rect::new(0, 0, 4, 3), Color::Magenta, 10);
        });
    }

    #[test]
    fn paint_card_top_shimmer_out_of_range_skips() {
        let backend = TestBackend::new(30, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| {
            // pos 远在范围外 → 直接 return
            paint_card_top_shimmer(f, Rect::new(0, 0, 30, 3), Color::Magenta, 1000);
        });
    }

    #[test]
    fn hit_test_card_outside_returns_none() {
        let card = StubCard::new(1, kinds::USER, 5, CardStreaming::Static);
        let rect = Rect::new(10, 10, 20, 10);
        // 坐标 (5, 5) 在 rect 外
        assert!(hit_test_card(&card, rect, 5, 5).is_none());
        // (35, 15) 也在 rect 外
        assert!(hit_test_card(&card, rect, 35, 15).is_none());
    }

    #[test]
    fn hit_test_card_header_returns_header() {
        let card = StubCard::new(1, kinds::USER, 5, CardStreaming::Static);
        let rect = Rect::new(10, 10, 20, 10);
        // (15, 10) 是 header 行
        assert_eq!(hit_test_card(&card, rect, 15, 10), Some(CardHit::Header));
    }

    #[test]
    fn hit_test_card_collapse_toggle() {
        let card = StubCard::new(1, kinds::USER, 5, CardStreaming::Static);
        let rect = Rect::new(10, 10, 20, 10);
        // 折叠箭头: x = 10 + 20 - 2 = 28, y = 10 + 10 - 1 = 19
        assert_eq!(hit_test_card(&card, rect, 28, 19), Some(CardHit::CollapseToggle));
    }

    #[test]
    fn hit_test_card_body_returns_body_with_inner_y() {
        let card = StubCard::new(1, kinds::USER, 5, CardStreaming::Static);
        let rect = Rect::new(10, 10, 20, 10);
        // (15, 13) 在 body 内, inner_y = 13 - (10 + 2) = 1
        match hit_test_card(&card, rect, 15, 13) {
            Some(CardHit::Body { inner_y }) => assert_eq!(inner_y, 1),
            other => panic!("expected Body {{ inner_y: 1 }}, got {:?}", other),
        }
    }

    #[test]
    fn hit_test_card_too_small_rect_returns_none() {
        let card = StubCard::new(1, kinds::USER, 5, CardStreaming::Static);
        // width < 3 → None
        assert!(hit_test_card(&card, Rect::new(0, 0, 2, 10), 0, 5).is_none());
        // height < 3 → None
        assert!(hit_test_card(&card, Rect::new(0, 0, 10, 2), 5, 0).is_none());
    }
}
