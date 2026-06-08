//! V42-B: 消息流卡片 hit-test (鼠标点击坐标 → 命中的 Card id)
//!
//! 替代 V40 的 `message_trace_row_map` 反查逻辑 (event/mod.rs 1844-1853)。
//!
//! ## 设计
//!
//! 重新计算每张卡片的 body_height, 累加得到每个卡片的屏幕 y 范围。
//! 给定 (x, y), 二分查找 y 落在哪张卡, 返回该卡 id。
//!
//! ## 与 V40 的对照
//!
//! - V40: 渲染时把 `(y, msg_idx, part_idx)` 写入 `message_trace_row_map`,
//!   hit-test 时 O(n) 扫描 row_map
//! - V42-B: 渲染时不写 state (保持 render pure), hit-test 自己重算
//!   O(n) 累加高度, 但 n 很小 (典型 10-100), 性能可接受
//!
//! ## 不写 state
//!
//! 与 `render_cards` 一致, hit_test 路径不修改 `AppState`。
//! 即使渲染和 hit-test 都在同一帧, 也保证一致性 (都基于同一份 CardStream 状态)。

use abacus_ui_kit::card_total_height;

use crate::tui::components::section_ctx::AppContext;
use crate::tui::state::AppState;

/// 给定屏幕坐标 (x, y) 反查命中的 Card id
///
/// ## 参数
/// - `state`: AppState (immutable 借用)
/// - `x`: 屏幕列坐标
/// - `y`: 屏幕行坐标
///
/// ## 返回
/// - `Some(card_id)`: 命中某张卡
/// - `None`: 不在任何卡片内 (空白行 / 边框外)
///
/// ## 不变量
///
/// 调用方 (event/mod.rs) 应当确保 (x, y) 在消息区 Rect 内, 否则返回 None。
/// 本函数不校验 area 边界, 由调用方负责。
pub fn card_hit_test(state: &AppState, x: u16, y: i32) -> Option<u64> {
    let ctx = AppContext::new(state);
    let inner_width = state.last_msg_area_width.borrow().saturating_sub(2);
    let mut current_y = *state.last_msg_area_y.borrow() as i32;
    let y_end = current_y + *state.last_msg_area_height.borrow() as i32;

    for card in state.cards.iter() {
        let id = card.id();
        let collapse = state.cards.collapse(id);
        let h = card_total_height(card.as_ref(), &ctx, inner_width, collapse) as i32;
        if h == 0 {
            continue;
        }
        if y >= current_y && y < current_y + h {
            // 命中 — 进一步校验 x 范围
            let x_start = *state.last_msg_area_x.borrow() as i32;
            let x_end = x_start + *state.last_msg_area_width.borrow() as i32;
            if (x as i32) >= x_start && (x as i32) < x_end {
                return Some(id);
            }
            return None;
        }
        current_y += h;
        if current_y >= y_end {
            break;
        }
    }
    None
}
