//! Team 模式 — 多 Agent 协作
//!
//! 布局与 Chat 一致：消息区 + 可选看板（角色/任务在看板 Tasks tab）
//! 布局: 顶栏(1行) + 消息区 + 输入框(自适应3/5/7行) + 底栏(1行)
//!
//! ## ⚠ 代码审查 @2025-01-23 (中等) — 详见 modes/chat.rs 顶部注释
//! 本文件 render() 与 chat.rs/meeting.rs 共享 ~25 行公共布局逻辑，建议抽取。

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use crate::tui::components;
use crate::tui::layout::{self, TerminalWidth};
use crate::tui::state::{AppState};

pub fn render(f: &mut Frame, state: &AppState, terminal_rows: u16) {
    // MD3：极小终端保护
    if components::render_min_terminal_warning(f) { return; }
    components::render_global_background(f, state);

    let input_h = layout::chat_input_height(terminal_rows);

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(input_h),
            Constraint::Length(1),
        ])
        .split(f.area());

    components::render_top_bar(f, state, root[0]);

    if state.panel_visible && !matches!(TerminalWidth::classify(root[1].width), TerminalWidth::Narrow) {
        let main = layout::body_with_panel(root[1]);
        components::render_messages_in_card(f, state, main.0, state.focus);
        components::render_panel(f, state, main.1);
        let input_area = ratatui::layout::Rect {
            x: main.0.x, width: main.0.width, ..root[2]
        };
        components::render_input_bar_focused(f, state, input_area, state.focus);
        // 命令提示框（面板下方，输入框右侧）
        let shortcuts_area = ratatui::layout::Rect {
            x: main.1.x, width: main.1.width, ..root[2]
        };
        components::render_shortcuts_hints(f, state, shortcuts_area);
    } else {
        components::render_messages_in_card(f, state, root[1], state.focus);
        let input_area = ratatui::layout::Rect {
            x: root[1].x, width: root[1].width, ..root[2]
        };
        components::render_input_bar_focused(f, state, input_area, state.focus);
    }

    components::render_status_bar(f, state, root[3]);
    // MD1+MD2：与 chat 模式一致的 overlay 入口（confirm_dialog + completion popup + toasts）
    // V32: 传 root[1] 让 completion popup 高度上限按消息区 45% 计算
    components::render_overlays(f, state, root[2], root[1]);
}

// V32: mock_data 已删（dead code，无生产调用）。
// Team 看板真实数据接入：EngineResponse 需扩展 team_state: Option<TeamState>
// 字段，run.rs 在 Team ApiResult::Ok 路径写 state.experts/tasks。Meeting 已通过
// run.rs:486 接入 meeting_experts 真实流。
