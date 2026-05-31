//! Clarify 模式 — 单 Agent 对话（需求澄清/快速验证/单轮问答）
//!
//! 设计规范来源: ABACUS-TUI-DESIGN-SPEC.md v1.0
//!
//! 布局: 顶栏(100%宽) + 对话区 + 输入区(7/5/4行) + 可选右侧面板(28%宽, Ctrl+B)
//!
//! Phase 3 去重：公共布局已抽取到 modes/common.rs，本模块仅保留模式特定逻辑。
//! 引用关系：被 modes/mod.rs::render 在 AbacusMode::Clarify 时调用

use ratatui::Frame;

use crate::tui::state::AppState;

pub fn render(f: &mut Frame, state: &AppState, terminal_rows: u16) {
    // Phase 3: 公共布局骨架（极小终端保护 + 背景 + 四段 Layout + top_bar）
    let Some(frame) = super::common::render_standard_frame(f, state, terminal_rows) else {
        return;
    };

    // 模式特定：消息区 + 输入框 + 面板（Clarify 无额外定制）
    let input_area = super::common::render_body_and_input(f, state, &frame);

    // 公共 overlay：status_bar + toasts + confirm + completion
    super::common::render_overlays(f, state, input_area, frame.body, frame.status);
}

// V32: mock_data 已删（dead code，无生产调用）。
// Team 看板真实数据接入：EngineResponse 需扩展 team_state: Option<TeamState>
// 字段，run.rs 在 Team ApiResult::Ok 路径写 state.experts/tasks。Meeting 已通过
// run.rs:486 接入 meeting_experts 真实流。
