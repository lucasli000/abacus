//! render_card_bar — 圆角卡片容器 + 色条卡片（panel 内容区复用）
//!
//! 提供 panel 风格的左侧 ┃ 色条渲染：
//! - [`render_card_bar`]：左色条 ┃ 风格（panel/CommandHint 内部统一视觉）
//!
//! ## 历史
//!
//! - 早期版本同时提供 `Card`（带阴影的圆角块容器），但无外部调用方（Reviewer C-7）。
//! - 2026-06-07 删除了未使用的 `Card` struct（112 行），只保留 `render_card_bar`。
//!
//! ## 引用关系
//!
//! - `render_card_bar` 为 `pub(super)`，供 `panel.rs:162` 调用（不暴露到 crate 外）。

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

/// V23: 给 area 加左侧 ┃ 色条, 返回剩余的内容区 Rect
/// 设计意图:
///   - 色条作为视觉锚点贯通整个内容高度 (panel "卡片型" 风格)
///   - 内容渲染代码不需要关心色条 — 关注点分离
///   - 三模式所有 PanelTab 内容区(overview/team_board/meeting_agenda/custom)统一调用
/// 引用关系: 被 `panel.rs` 调用（Clarify/Plan/Team/Meeting 四分支）
/// 生命周期: 每帧渲染; 不持有状态
pub(super) fn render_card_bar(f: &mut ratatui::Frame, theme: &abacus_ui_kit::Theme, area: Rect) -> Rect {
    let split_h = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Horizontal)
        .constraints([
            ratatui::layout::Constraint::Length(1),  // ┃ 色条列
            ratatui::layout::Constraint::Min(1),     // 内容列
        ])
        .split(area);

    let bar_style = Style::default().fg(theme.primary);
    let bar_lines: Vec<Line> = (0..area.height)
        .map(|_| Line::styled("▏", bar_style))
        .collect();
    f.render_widget(Paragraph::new(bar_lines), split_h[0]);

    split_h[1]
}
