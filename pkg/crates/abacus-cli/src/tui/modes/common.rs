//! 公共渲染布局 — 四模式共享的 frame 结构 + overlay
//!
//! Phase 3 去重：Clarify/Plan/Team/Meeting 四模式 render() 共享 ~25 行相同代码
//! （极小终端保护 + 全局背景 + 四段 Layout + top_bar + status_bar + overlays）。
//! 本模块收口为 render_standard_frame + render_overlays 两个入口。
//!
//! 引用关系：被 modes/clarify.rs、plan.rs、team.rs、meeting.rs 的 render() 调用
//! 生命周期：纯渲染函数，无状态，无副作用

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::tui::components;
use crate::tui::layout::{self, TerminalWidth};
use crate::tui::state::AppState;

/// 标准四段布局结果
pub struct StandardFrame {
    /// 顶栏区域（已渲染 top_bar）
    pub top: Rect,
    /// 消息/内容区域
    pub body: Rect,
    /// 输入框区域
    pub input: Rect,
    /// 底栏区域
    pub status: Rect,
}

/// 渲染标准 frame 骨架：极小终端保护 + 全局背景 + 四段 Layout + top_bar
///
/// 返回 None 表示终端过小已渲染警告，调用方应直接 return。
/// 返回 Some(StandardFrame) 时 top_bar 已渲染，调用方负责 body/input/status 内容。
pub fn render_standard_frame(f: &mut Frame, state: &AppState, terminal_rows: u16) -> Option<StandardFrame> {
    if components::render_min_terminal_warning(f) {
        return None;
    }
    components::render_global_background(f, state);

    let input_h = layout::chat_input_height(terminal_rows);

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),       // 顶栏
            Constraint::Min(6),          // 消息区
            Constraint::Length(input_h), // 输入框
            Constraint::Length(1),       // 底栏
        ])
        .split(f.area());

    components::render_top_bar(f, state, root[0]);

    Some(StandardFrame {
        top: root[0],
        body: root[1],
        input: root[2],
        status: root[3],
    })
}

/// 渲染公共 overlay 层：status_bar + toasts + confirm_dialog + completion popup
///
/// 引用关系：对应原各模式 render 末尾的 render_status_bar + render_overlays 调用
pub fn render_overlays(f: &mut Frame, state: &AppState, input_area: Rect, body_area: Rect, status_area: Rect) {
    components::render_status_bar(f, state, status_area);
    components::render_overlays(f, state, input_area, body_area);
}

/// 渲染宽屏/窄屏分支的消息区 + 输入框 + 面板
///
/// 公共逻辑：panel_visible && 非 Narrow → body_with_panel 分割；否则全宽消息区
/// 返回实际用于 overlay 定位的 input_area（可能是左半区宽度）
pub fn render_body_and_input(f: &mut Frame, state: &AppState, frame: &StandardFrame) -> Rect {
    if state.panel_visible && !matches!(TerminalWidth::classify(frame.body.width), TerminalWidth::Narrow) {
        let main = layout::body_with_panel(frame.body);
        components::render_messages_in_card(f, state, main.0, state.focus);
        components::render_panel(f, state, main.1);

        let input_area = Rect {
            x: main.0.x, width: main.0.width, ..frame.input
        };
        components::render_input_bar_focused(f, state, input_area, state.focus);

        let shortcuts_area = Rect {
            x: main.1.x, width: main.1.width, ..frame.input
        };
        components::render_shortcuts_hints(f, state, shortcuts_area);
        input_area
    } else {
        components::render_messages_in_card(f, state, frame.body, state.focus);
        let input_area = Rect {
            x: frame.body.x, width: frame.body.width, ..frame.input
        };
        components::render_input_bar_focused(f, state, input_area, state.focus);
        input_area
    }
}
