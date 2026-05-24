//! Plan 模式 — Planner agent 独立角色
//!
//! V33 新增：用户从 Clarify 转入 Plan 时，Planner agent 把澄清后的需求
//! 拆解为结构化 TaskSpec[]，作为 Team 模式输入。
//!
//! Phase 3 去重：公共布局已抽取到 modes/common.rs，本模块仅保留模式特定逻辑。
//!
//! ## 引用关系
//! - state.mode == AbacusMode::Plan 时本模块的 render 被调用
//! - PlannerAgent 路径实装位置：`tui/api/mod.rs::send_planner_message_streaming`
//!
//! ## 布局
//! 与 Clarify/Team/Meeting 一致的四段：顶栏(1) + 消息区(min 6) + 输入框(自适应) + 底栏(1)
//! Panel 显示 Plan 输出的 TaskSpec 预览（id/title/dependencies）

use ratatui::Frame;

use crate::tui::state::AppState;

pub fn render(f: &mut Frame, state: &AppState, terminal_rows: u16) {
    let Some(frame) = super::common::render_standard_frame(f, state, terminal_rows) else {
        return;
    };
    let input_area = super::common::render_body_and_input(f, state, &frame);
    super::common::render_overlays(f, state, input_area, frame.body, frame.status);
}
