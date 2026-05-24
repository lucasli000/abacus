//! Meeting 模式 — 多专家会议
//!
//! 布局与 Chat 一致：消息区 + 可选看板
//! 布局: 顶栏(1行) + 消息区 + 输入框(自适应3/5/7行) + 底栏(1行)
//!
//! Phase 3 去重：公共布局已抽取到 modes/common.rs。
//! Meeting 的 panel 内容（Experts tab + 置信度）由 render_panel 根据 state.mode 路由。

use ratatui::Frame;

use crate::tui::state::AppState;

pub fn render(f: &mut Frame, state: &AppState, terminal_rows: u16) {
    let Some(frame) = super::common::render_standard_frame(f, state, terminal_rows) else {
        return;
    };
    // Meeting 模式：消息区 + 面板的 content_area 传递给 input
    // 注意：原 meeting 的 input_area 宽度跟随 content_area（非 panel 区），
    //       common::render_body_and_input 已正确实现此逻辑
    let input_area = super::common::render_body_and_input(f, state, &frame);
    super::common::render_overlays(f, state, input_area, frame.body, frame.status);
}

// V32: mock_data 已删（dead code，无生产调用）。
// Team 看板真实数据接入：EngineResponse 需扩展 team_state: Option<TeamState>
// 字段，run.rs 在 Team ApiResult::Ok 路径写 state.experts/tasks。Meeting 已通过
// run.rs:486 接入 meeting_experts 真实流。
