//! Clarify 模式 — 单 Agent 对话（需求澄清/快速验证/单轮问答）
//!
//! 设计规范来源: ABACUS-TUI-DESIGN-SPEC.md v1.0
//!
//! 布局: 顶栏(100%宽) + 对话区 + 输入区(7/5/4行) + 可选右侧面板(28%宽, Ctrl+B)
//!
//! ## ⚠ 代码审查 @2025-01-23 (中等)
//! Chat/Team/Meeting 三种 render() 函数有大量重复代码（约 25 行完全相同的布局逻辑：
//! render_min_terminal_warning → chat_input_height → 四段 Layout → render_top_bar →
//! 窄屏/宽屏分支 → render_input_bar_focused → render_status_bar → render_overlays）。
//! 三种模式仅在 Panel 内容上不同 (render_panel_overview / render_panel_team_board /
//! render_panel_meeting_agenda)。
//!
//! 建议: 抽取 render_common_layout(f, state, terminal_rows, panel_fn) 公共函数，
//! 三个模式各传不同的 panel 渲染闭包，消除 3×25=75 行重复。

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use crate::tui::components;
use crate::tui::layout::{self, TerminalWidth};
use crate::tui::state::{AppState};

// V7 完善：z_index 渲染顺序参考（const 引用让 z_index 模块被实际使用，避免死码）
// 实际渲染顺序由 render() 函数内部调用次序保证：背景 → 卡片 → 浮动 → 模态 → 覆盖 → 光标
#[allow(dead_code)]
const RENDER_LAYER_ORDER: [u8; 10] = [
    crate::tui::theme::z_index::GLOBAL_BG,
    crate::tui::theme::z_index::CARD_SHADOW,
    crate::tui::theme::z_index::CARD_BG,
    crate::tui::theme::z_index::CARD_BORDER,
    crate::tui::theme::z_index::CARD_CONTENT,
    crate::tui::theme::z_index::STATE_HIGHLIGHT,
    crate::tui::theme::z_index::FLOATING,
    crate::tui::theme::z_index::MODAL,
    crate::tui::theme::z_index::OVERLAY,
    crate::tui::theme::z_index::CURSOR,
];

pub fn render(f: &mut Frame, state: &AppState, terminal_rows: u16) {
    // ## ⚠ 代码审查 @2025-01-23 (低)
    // render_min_terminal_warning 返回 true 时静默 return，跳过后续
    // render_overlays / render_status_bar，包括 toast 通知。用户在极窄终端
    // （<10 行或 <40 列）时看不到任何错误/状态反馈。可考虑至少在 return 前
    // 执行 render_toasts 让关键通知不受终端尺寸影响。
    // 极小终端保护：统一走 components::render_min_terminal_warning
    if components::render_min_terminal_warning(f) { return; }
    components::render_global_background(f, state);

    // 输入框高度：根据终端自适应（≥40行=7, 20-39行=5, <20行=3）
    let input_h = layout::chat_input_height(terminal_rows);

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),          // 顶栏
            Constraint::Min(6),             // 消息区
            Constraint::Length(input_h),    // 输入框
            Constraint::Length(1),          // 底栏
        ])
        .split(f.area());

    components::render_top_bar(f, state, root[0]);

    // 窄屏隐藏看板，保证消息区域宽度
    if state.panel_visible && !matches!(TerminalWidth::classify(root[1].width), TerminalWidth::Narrow) {
        let main = layout::body_with_panel(root[1]);
        components::render_messages_in_card(f, state, main.0, state.focus);
        components::render_panel(f, state, main.1);

        let input_area = ratatui::layout::Rect {
            x: main.0.x, width: main.0.width, ..root[2]
        };
        components::render_input_bar_focused(f, state, input_area, state.focus);

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
    // 三模式共用 overlay 入口：toasts + confirm_dialog + completion popup
    // root[1] = 消息区（用于限制 completion popup 高度上限 ≤ 45%）；root[2] = 输入框
    components::render_overlays(f, state, root[2], root[1]);
}

// V32: mock_data 已删（dead code，无生产调用）。
// Team 看板真实数据接入：EngineResponse 需扩展 team_state: Option<TeamState>
// 字段，run.rs 在 Team ApiResult::Ok 路径写 state.experts/tasks。Meeting 已通过
// run.rs:486 接入 meeting_experts 真实流。
