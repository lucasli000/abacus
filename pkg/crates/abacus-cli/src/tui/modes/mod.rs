//! Abacus TUI Modes — 三大交互模式
//!
//! 设计规范来源: ABACUS-TUI-DESIGN-SPEC.md v1.0

pub mod analyzer;
pub mod clarify;
pub mod common;
pub mod meeting;
pub mod plan;
pub mod team;

use crate::tui::state::{AppState, AbacusMode};
use ratatui::Frame;

pub fn render(f: &mut Frame, state: &AppState, terminal_rows: u16) {
    match state.mode {
        AbacusMode::Clarify => clarify::render(f, state, terminal_rows),
        AbacusMode::Plan => plan::render(f, state, terminal_rows),
        AbacusMode::Team => team::render(f, state, terminal_rows),
        AbacusMode::Meeting => meeting::render(f, state, terminal_rows),
    }

    // M1: 删除空的 info_panel_auto_open 占位分支——
    // 实际"自动打开 panel"的副作用应在 AppState 写入处直接 toggle panel_visible，
    // 在不可变 render 阶段无法做到（需要 &mut state），原注释承认了这点

    // 设置模态框覆盖层
    if state.show_settings {
        let area = f.area();
        crate::tui::components::render_settings_modal(f, state, area);
    }
}

// V32: mock_data 路由已删（dead code，无生产调用）
