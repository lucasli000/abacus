//! V42-B: 消息流卡片渲染入口
//!
//! 替代 V40 的 `render_messages_in_card` (1439 行)。
//! 通过 [`abacus_ui_kit::render_card`] + [`CardStream`] 通用路径渲染。
//!
//! ## 渲染流程
//!
//! 1. 画外框 (圆角边框 + 背景)
//! 2. 流式期间: 顶部 shimmer 光带
//! 3. 遍历 `state.cards` 的每张卡:
//!    - 调 `card_total_height` 算高度
//!    - 按 scroll offset 决定是否在可见区
//!    - 调 `render_card` 画单卡
//! 4. 缓存 item_areas 到 `state.scroll_layout` 供 hit-test 使用
//!
//! ## 与 V40 的对照
//!
//! | V40 | V42-B |
//! |-----|-------|
//! | `messages.rs` 1439 行 | `render.rs` ~150 行 |
//! | `build_message_lines` 缓存 L0/L1/L2 三级 | 每次直接 render, 缓存 item_areas |
//! | `cached_msg_rows` + `estimate_msg_rows` | `card_total_height` 精确返回 |
//! | `message_trace_row_map` | `ScrollLayout.item_areas` |
//! | 4 种角色 if/else 分支 | 4 个 Card 各自 render_body |

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Borders, Widget};

use abacus_ui_kit::hooks::ShimmerPhase;
use abacus_ui_kit::prelude::*;

use crate::tui::components::section_ctx::AppContext;
use crate::tui::state::AppState;

/// V42-B 消息流渲染入口 —— 替代 `render_messages_in_card`
///
/// ## 参数
/// - `f`: ratatui Frame
/// - `state`: AppState (immutable 借用, 通过 `RefCell` 内部写 last_msg_area_* 缓存)
/// - `area`: 消息区 Rect
/// - `_focus`: 当前焦点 (V42-B 消息区不参与焦点循环, 保留参数兼容旧调用)
pub fn render_cards(f: &mut Frame, state: &AppState, area: Rect, _focus: crate::tui::state::Focus) {
    // 1. 消息区外框 (圆角边框 + 主题背景)
    let msg_block = Block::default()
        .border_type(BorderType::Rounded)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(state.theme.border))
        .style(Style::default().bg(state.theme.bg));
    let inner = msg_block.inner(area);
    msg_block.render(area, f.buffer_mut());

    // V42-B: 缓存 inner Rect 供 hit_test 使用 (通过 RefCell 内部写)
    *state.last_msg_area_x.borrow_mut() = inner.x;
    *state.last_msg_area_y.borrow_mut() = inner.y;
    *state.last_msg_area_width.borrow_mut() = inner.width;
    *state.last_msg_area_height.borrow_mut() = inner.height;

    // 2. 流式 shimmer 顶部光带
    let is_streaming = state.cards.active_id().is_some();
    if is_streaming {
        let tick = state.anim_tick.get();
        let shimmer_pos = ShimmerPhase::compute(tick, 50, 3500, 8, inner.width as u16);
        // 用 active 卡的主色 (如有), 否则 border 色
        let color = state
            .cards
            .active_id()
            .and_then(|id| state.cards.card(id).map(|c| default_color_for_kind(c.kind(), &state.theme)))
            .unwrap_or(state.theme.border);
        paint_card_top_shimmer(f, area, color, shimmer_pos);
    }

    // 3. 构造 SectionContext (一次性, 复用给所有 Card)
    let ctx = AppContext::new(state);

    // 4. 遍历卡片, 按 scroll offset 画可见部分
    let scroll_offset = state.message_scroll_y;
    let mut y = inner.y;
    let mut skipped = 0u16;

    for card in state.cards.iter() {
        let id = card.id();
        let collapse = state.cards.collapse(id);
        let h = card_total_height(card.as_ref(), &ctx, inner.width, collapse);
        if h == 0 {
            continue;
        }

        // scroll: 跳过 scroll_offset 行
        if skipped < scroll_offset {
            skipped = skipped.saturating_add(h);
            continue;
        }

        if y >= inner.y + inner.height {
            break; // 超出可见区
        }

        // 计算实际渲染 Rect (可能被底部截断)
        let available_h = inner.y + inner.height - y;
        let actual_h = h.min(available_h);
        if actual_h == 0 {
            break;
        }
        let rect = Rect::new(inner.x, y, inner.width, actual_h);

        // 计算 shimmer 位置 (仅 active 卡)
        let shimmer_pos = if state.cards.active_id() == Some(id) {
            let tick = state.anim_tick.get();
            ShimmerPhase::compute(tick, 50, 3500, 8, inner.width as u16)
        } else {
            -999 // sentinel: 关闭 shimmer
        };

        render_card(f, card.as_ref(), &ctx, rect, collapse, shimmer_pos);

        y = y.saturating_add(actual_h);
    }

    // 5. 状态指示 (V40 顶部右下 "压缩中" 红条 — V42-B 暂不实现, 等 Phase 14 清理时统一)
}
