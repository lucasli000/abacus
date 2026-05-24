//! Plan 模式 — Planner agent 独立角色
//!
//! V33 新增：用户从 Clarify 转入 Plan 时，Planner agent 把澄清后的需求
//! 拆解为结构化 TaskSpec[]，作为 Team 模式输入。
//!
//! ## 引用关系
//! - state.mode == AbacusMode::Plan 时本模块的 render 被调用
//! - PlannerAgent 路径实装位置：`tui/api/mod.rs::send_planner_message_streaming`
//!   （V35-2 起通过 RequestContext.system_prompt_override / tool_filter /
//!   prefix_assistant_content 三字段联动实现，无独立 agent crate）
//! - 输出 TaskSpec[] 通过 ModeArtifact::PlanTasks 透传给 Team
//!   （写入端：slash_commands.rs::extract_plan_tasks_from_messages → set_mode_artifact；
//!    消费端：event/mod.rs::switch_mode 在 Plan→Team 转换时 take()）
//!
//! ## V34+ 深度集成方向（未实装）
//! 抽出独立 `abacus-orchestrator/src/specialist/planner.rs` 走 Specialist trait
//! 框架，对齐 Team mode 的多专家协作；当前 cli 直注入方案是过渡，足够 minimum viable。
//!
//! ## 布局
//! 与 Clarify/Team/Meeting 一致的四段：顶栏(1) + 消息区(min 6) + 输入框(自适应) + 底栏(1)
//! Panel 显示 Plan 输出的 TaskSpec 预览（id/title/dependencies）

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use crate::tui::components;
use crate::tui::layout::{self, TerminalWidth};
use crate::tui::state::AppState;

pub fn render(f: &mut Frame, state: &AppState, terminal_rows: u16) {
    if components::render_min_terminal_warning(f) {
        return;
    }
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
    components::render_overlays(f, state, root[2], root[1]);
}
