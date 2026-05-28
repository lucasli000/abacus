//! Abacus TUI Layout — 响应式布局计算
//!
//! 设计规范来源: ABACUS-TUI-DESIGN-SPEC.md v1.0
//!
//! 三大模式布局:
//!   Chat: 顶栏 + 对话区 + 输入区(7/5/4行) + 可选右侧面板(28%)
//!   Team: 左侧角色栏(20%) + 中间任务看板(55%) + 右侧交互区(25%)
//!   Meeting: 左侧专家栏(20%) + 中间对话区(55%) + 右侧会议面板(25%)

use ratatui::layout::{Constraint, Direction, Layout, Rect};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 通用布局函数
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 面板宽度自适应百分比（窄 22% / 标准 25% / 宽 28%）— 单一真相
///
/// 引用关系：被 body_with_panel + event::handle_mouse 共用，避免
/// 鼠标判定区与渲染布局百分比漂移（EV5 修复前 mouse 硬编码 28%）
pub fn panel_pct_for_width(cols: u16) -> u16 {
    match TerminalWidth::classify(cols) {
        TerminalWidth::Narrow => 22,
        TerminalWidth::Normal => 25,
        TerminalWidth::Wide => 28,
    }
}

/// 主体 + 右侧面板 (72% / 28%)
pub fn body_with_panel(area: Rect) -> (Rect, Rect) {
    let panel_pct = panel_pct_for_width(area.width);
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(100 - panel_pct),
            Constraint::Percentage(panel_pct),
        ])
        .split(area);
    (parts[0], parts[1])
}

/// 面板内部: 状态摘要 + 内容
pub fn panel_inner(area: Rect) -> (Rect, Rect) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);
    (parts[0], parts[1])
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Chat 模式布局
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Chat 模式输入区高度 (根据终端高度自适应)
/// 三档：
/// - Tall  (≥40 行)：6 行 = 顶栏(1)+文本(2)+底栏(1)+边框(2)（V14：去掉冗余留白，Ready/Enter 行贴底）
/// - Normal(20-39)：5 行 = 顶栏(1)+文本(1)+底栏(1)+边框(2)
/// - Short (<20)  ：3 行 = 文本(1)+边框(2)（极窄模式省略顶/底栏）
/// 2026-05-28: 自适应高度 — 按 input 行数动态扩展（2→8 行文本区）
/// 参数 input_lines: 当前输入的行数（0 = 空输入）
/// 参数 prev_h: 上一帧的高度（用于 hysteresis：只涨不跌，除非 input 回到 ≤2 行）
/// 结构：border(2) + status_line(1) + text_lines(N) + info_line(1)
///       即总高 = N + 4（最小 5，最大 12）
pub fn chat_input_height_adaptive(terminal_rows: u16, input_lines: usize) -> u16 {
    if matches!(TerminalHeight::classify(terminal_rows), TerminalHeight::Short) {
        return 3;
    }
    // 文本行上限：终端高度的 40%，且不超过 8 行
    let max_text_lines = ((terminal_rows as usize) * 40 / 100).min(8).max(2);
    let desired_text_lines = input_lines.max(2).min(max_text_lines);
    // 总高 = text_lines + 4 (border×2 + status + info)
    let h = (desired_text_lines + 4) as u16;
    h.min(terminal_rows / 2) // 绝不超过屏幕一半
}

pub fn chat_input_height(terminal_rows: u16) -> u16 {
    // 向后兼容：无 input 行数时用默认值
    chat_input_height_adaptive(terminal_rows, 2)
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Team 模式布局
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Team 三栏布局: 角色栏(20%) + 任务看板(55%) + 交互区(25%)
pub fn team_three_col(area: Rect) -> (Rect, Rect, Rect) {
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(55),
            Constraint::Percentage(25),
        ])
        .split(area);
    (parts[0], parts[1], parts[2])
}

/// 任务看板内部: 状态栏 + 任务卡片列表
pub fn team_kanban_inner(area: Rect) -> (Rect, Rect) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(3)])
        .split(area);
    (parts[0], parts[1])
}

/// Team 交互区内部: 对话 + 通知/日志
pub fn team_interaction_inner(area: Rect) -> (Rect, Rect) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);
    (parts[0], parts[1])
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Meeting 模式布局
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Meeting 三栏布局: 专家栏(20%) + 对话区(55%) + 会议面板(25%)
pub fn meeting_three_col(area: Rect) -> (Rect, Rect, Rect) {
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(55),
            Constraint::Percentage(25),
        ])
        .split(area);
    (parts[0], parts[1], parts[2])
}

/// 会议面板内部: 议题 + 议程 + 结论 + 待办 + 投票
pub fn meeting_panel_inner(area: Rect) -> (Rect, Rect, Rect, Rect, Rect) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(2),
            Constraint::Min(2),
            Constraint::Min(2),
            Constraint::Min(2),
        ])
        .split(area);
    (parts[0], parts[1], parts[2], parts[3], parts[4])
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 响应式适配
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 终端宽度判断: 窄屏(<90) 标准(90-109) 宽屏(>=110)
/// F7 历史调整：原阈值 <80/>=100 → 现 <90/>=110，避免临界宽度面板挤压
pub enum TerminalWidth {
    Narrow,   // < 90 列
    Normal,   // 90-109 列
    Wide,     // >= 110 列
}

impl TerminalWidth {
    pub fn classify(cols: u16) -> Self {
        // F7 完善：阈值放宽（< 80 → < 90；>= 100 → >= 110）避免临界宽度面板挤压
        if cols < 90 {
            Self::Narrow
        } else if cols >= 110 {
            Self::Wide
        } else {
            Self::Normal
        }
    }
}

/// 终端高度判断: 影响顶栏行数和输入区高度
pub enum TerminalHeight {
    Short,    // < 20 行
    Normal,   // 20-39 行
    Tall,     // >= 40 行
}

impl TerminalHeight {
    pub fn classify(rows: u16) -> Self {
        if rows < 20 {
            Self::Short
        } else if rows >= 40 {
            Self::Tall
        } else {
            Self::Normal
        }
    }
}
