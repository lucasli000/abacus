//! Extra render functions: expert list, task kanban, global background, shortcuts hints.
//!
//! Extracted from mod.rs to reduce file size. All functions are `pub(super)` and
//! re-exported via `pub use extras::*` in the parent module.
//!
//! ## References
//! - Consumed by: `crate::tui::components` (re-export) → layout/rendering code
//! - Depends on: `AppState`, `ExpertStatus`, `TaskStatus`, `Focus`, `Theme`, `TextRole`

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::tui::i18n::t;
use crate::tui::state::{AppState, ExpertStatus, TaskStatus};
use crate::tui::theme::TextRole;

// ════════════════════════════════════════════════════════════════
// ExpertList — 专家列表
// ════════════════════════════════════════════════════════════════

pub fn render_expert_list(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .title(t("label.expert"))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(state.theme.border));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    for expert in &state.experts {
        let (status_icon, status_color) = match expert.status {
            ExpertStatus::Active => ("●", state.theme.success),
            ExpertStatus::Idle => ("◌", state.theme.muted),
            ExpertStatus::Done => ("✓", state.theme.accent),
        };

        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", status_icon),
                Style::default().fg(status_color),
            ),
            Span::styled(
                &expert.name,
                Style::default()
                    .fg(state.theme.expert)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                &expert.domain,
                Style::default()
                    .fg(state.theme.muted)
                    .add_modifier(Modifier::DIM),
            ),
        ]));

        // V28.7: confidence == 0.0 → "未评估"，>0 → 百分比（orchestrator 暂无评估机制时不造伪数据）
        let conf_label = if expert.confidence > 0.0 {
            format!("{}: {:.0}%", t("field.confidence"), expert.confidence * 100.0)
        } else {
            format!("{}: —", t("field.confidence"))
        };
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(conf_label, Style::default().fg(state.theme.muted)),
        ]));

        lines.push(Line::raw(""));
    }

    if lines.is_empty() {
        lines.push(Line::styled(
            t("empty.experts"),
            Style::default().fg(state.theme.muted),
        ));
        lines.push(Line::styled(
            t("empty.invite_hint"),
            state.theme.text_style(TextRole::Caption),
        ));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

// ════════════════════════════════════════════════════════════════
// TaskKanban — 任务看板 (Team 模式)
// ════════════════════════════════════════════════════════════════

pub fn render_task_kanban(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .title(t("label.kanban"))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(state.theme.mode));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    if state.tasks.is_empty() {
        lines.push(Line::styled(
            t("empty.tasks"),
            Style::default().fg(state.theme.muted),
        ));
    } else {
        for task in &state.tasks {
            let (status_icon, status_color) = match task.status {
                TaskStatus::Pending => ("◌", state.theme.muted),
                TaskStatus::InProgress => ("●", state.theme.success),
                TaskStatus::Done => ("✓", state.theme.accent),
                TaskStatus::Blocked => ("⚠", state.theme.error),
            };

            let progress_filled = (task.progress as usize * 10 / 100).min(10);
            let progress_empty = 10 - progress_filled;
            let progress_text = format!("{}%", task.progress);

            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", status_icon),
                    Style::default().fg(status_color),
                ),
                Span::styled(
                    &task.title,
                    Style::default()
                        .fg(state.theme.text)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));

            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    format!("{}: {}", t("field.owner"), task.assignee),
                    Style::default().fg(state.theme.muted),
                ),
                Span::raw("  ["),
                Span::styled(
                    "█".repeat(progress_filled),
                    Style::default().fg(state.theme.accent),
                ),
                Span::styled(
                    "░".repeat(progress_empty),
                    Style::default().fg(state.theme.muted),
                ),
                Span::styled(
                    format!("] {}", progress_text),
                    Style::default().fg(state.theme.muted),
                ),
            ]));

            if !task.deps.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(
                        format!("{}: {}", t("field.deps"), task.deps.join(", ")),
                        Style::default()
                            .fg(state.theme.muted)
                            .add_modifier(Modifier::DIM),
                    ),
                ]));
            }

            lines.push(Line::raw(""));
        }
    }

    f.render_widget(Paragraph::new(lines), inner);
}

// ════════════════════════════════════════════════════════════════
// 全局背景渲染
// ════════════════════════════════════════════════════════════════
pub fn render_global_background(f: &mut ratatui::Frame, state: &AppState) {
    // 性能优化：仅首帧或主题切换时重刷全屏背景
    if state.last_rendered_theme.get() == Some(state.theme.name) {
        return;
    }
    state.last_rendered_theme.set(Some(state.theme.name));
    // 直接设置 buffer 背景色（零分配，比 Paragraph 快 10x）
    let area = f.area();
    let bg = state.theme.bg;
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut f.buffer_mut()[(x, y)];
            cell.set_symbol(" ");
            cell.set_bg(bg);
            cell.set_fg(state.theme.text);
        }
    }
}

// ════════════════════════════════════════════════════════════════
/// 命令行提示面板（输入框右侧，看板打开时显示，圆角边框，支持滚动）
pub fn render_shortcuts_hints(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    if area.width < 14 || area.height < 4 {
        return;
    }

    // V26: 焦点反馈对称化——上边框 primary, 其他三边 border (与 panel 统一)
    let is_focused = state.focus == crate::tui::state::Focus::CommandHint;
    let block = Block::default()
        .border_type(BorderType::Rounded)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(state.theme.border));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // V26.1: 同 panel — 缩小 area 避免覆盖角字符 ╭╮
    if is_focused && area.width >= 3 {
        let mut top_style = Style::default().fg(state.theme.primary);
        if state.focus_pulsing() {
            top_style = top_style.add_modifier(Modifier::BOLD);
        }
        let top_segment = Rect {
            x: area.x.saturating_add(1),
            y: area.y,
            width: area.width.saturating_sub(2),
            height: 1,
        };
        // V28.6 (PR12-1 续): 与 panel 焦点反馈对称, 上边框粗线 ━ 与看板内 ┃ 色条同属
        //   box drawings heavy 家族, 字符粗细 + primary 色一致, 视觉锚点统一
        let top_overlay = Block::default()
            .borders(Borders::TOP)
            .border_type(BorderType::Thick)
            .border_style(top_style);
        f.render_widget(top_overlay, top_segment);
    }

    let all_commands = &state.commands;

    let cmd_style = Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(state.theme.muted);
    let sep_style = Style::default().fg(state.theme.border);
    // V13: 选中行用 accent 反色 + BOLD（仅焦点时高亮）
    let selected_cmd_style = Style::default()
        .fg(state.theme.bg)
        .bg(state.theme.accent)
        .add_modifier(Modifier::BOLD);
    let selected_desc_style = Style::default()
        .fg(state.theme.bg)
        .bg(state.theme.accent);

    // 计算可见行数（内框高度 - 标题行 - 底部状态行 = 内容行数）
    let content_rows = inner.height.saturating_sub(2) as usize;
    if content_rows == 0 {
        return;
    }

    // 自适应布局：双列（宽≥22）/ 单列（窄屏）
    let is_wide = inner.width >= 22;
    let cols_per_row = if is_wide { 2 } else { 1 };
    let total_rows = (all_commands.len() + cols_per_row - 1) / cols_per_row;
    let max_scroll = total_rows.saturating_sub(content_rows);
    let scroll = state.cmd_scroll.min(max_scroll);

    let mut lines: Vec<Line> = Vec::new();

    // V13: 标题加焦点提示
    let title = if is_focused {
        t("cmd.title_focused")
    } else {
        t("cmd.title_unfocused")
    };
    lines.push(Line::from(vec![
        Span::styled(title.to_string(), Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
    ]));

    let sel = state.cmd_selected.min(all_commands.len().saturating_sub(1));

    // V28.7: ── 双列对齐计算（修复上下行 │ 分界不齐）──
    // 引用关系：
    //   - 输入：inner.width（外层 block.inner 已减边框）、is_wide（是否双列）
    //   - 输出：left_col_w 固定左列总显示宽度
    //   - 分界 "  │  " 显示宽度恒为 5（│ 是单列字符）
    // 设计：左列 cmd_part(▶ /xxx) + " " + desc 按 display_width 截断+补白到 left_col_w，
    //   保证每行进入分界 │ 之前的视觉宽度恒等→分界自然对齐
    use crate::tui::util::{display_width, truncate_to_width};
    const SEP_DISPLAY_WIDTH: usize = 5; // "  │  "
    let inner_w = inner.width as usize;
    let left_col_w: usize = if is_wide {
        // 双列：减分界宽度后均分，左列略宽 1 列吸收奇数余数
        let remain = inner_w.saturating_sub(SEP_DISPLAY_WIDTH);
        remain.saturating_sub(remain / 2).max(8)
    } else {
        inner_w
    };

    // 把 (cmd, desc, is_sel) 渲染为 [cmd Span, desc Span, padding Span]，三段累计 display_width == col_w
    // padding 用 Span::raw 不染色——避免选中行的 bg 染到尾部空白产生视觉拖尾
    let render_cell = |cmd: &str, desc: &str, marker: &str, is_sel: bool, col_w: usize| -> Vec<Span<'static>> {
        let cmd_part = format!("{} {}", marker, cmd);
        let cmd_w = display_width(&cmd_part);
        // desc 可用宽度 = col_w - cmd_w - 1（cmd 与 desc 之间的空格）
        let desc_max = col_w.saturating_sub(cmd_w + 1);
        let desc_text = if display_width(desc) > desc_max {
            // 截断 + 省略号（… 占 1 列）
            let cut = desc_max.saturating_sub(1).max(1);
            let mut t = truncate_to_width(desc, cut);
            t.push('…');
            t
        } else {
            desc.to_string()
        };
        let used_w = cmd_w + 1 + display_width(&desc_text);
        let pad_w = col_w.saturating_sub(used_w);
        let (cs, ds) = if is_sel { (selected_cmd_style, selected_desc_style) } else { (cmd_style, desc_style) };
        let mut out = vec![
            Span::styled(cmd_part, cs),
            Span::styled(format!(" {}", desc_text), ds),
        ];
        if pad_w > 0 {
            out.push(Span::raw(" ".repeat(pad_w)));
        }
        out
    };

    // V32 · 填充 cmd_row_map：每个内容行的屏幕 y 与起始 cmd_idx 关系，给鼠标点击反查用
    // 渲染前清空旧映射避免悬挂；inner.y 已是 block 内框起点，加偏移得到屏幕 y
    {
        let mut m = state.cmd_row_map.borrow_mut();
        m.clear();
    }
    // 标题行已 push，所以内容行从 inner.y + 1 开始
    let content_y_base = inner.y + 1;

    // 内容行（自适应双列/单列）
    for row_idx in 0..content_rows {
        let cmd_idx = (scroll + row_idx) * cols_per_row;
        if cmd_idx >= all_commands.len() {
            break;
        }
        // 行 row_idx 的屏幕 y
        let screen_y = content_y_base.saturating_add(row_idx as u16);
        state.cmd_row_map.borrow_mut().push((screen_y, cmd_idx));

        let (cmd1, desc1) = &all_commands[cmd_idx];
        let is_sel1 = is_focused && cmd_idx == sel;
        let marker1 = if is_sel1 { "▶" } else { " " };
        let mut spans: Vec<Span<'static>> = render_cell(cmd1, desc1, marker1, is_sel1, left_col_w);

        if is_wide && cmd_idx + 1 < all_commands.len() {
            let (cmd2, desc2) = &all_commands[cmd_idx + 1];
            let is_sel2 = is_focused && cmd_idx + 1 == sel;
            let marker2 = if is_sel2 { "▶" } else { " " };
            spans.push(Span::styled("  │  ", sep_style));
            // 右列宽度：剩余可用 = inner_w - left_col_w - sep_width
            let right_col_w = inner_w.saturating_sub(left_col_w + SEP_DISPLAY_WIDTH);
            spans.extend(render_cell(cmd2, desc2, marker2, is_sel2, right_col_w));
        } else if is_wide {
            // 双列模式但右列无内容（命令总数为奇数最后一行）：补满左列即可，
            // 不画分界——避免 "│" 后悬空看着突兀。render_cell 已 padding 到 left_col_w。
        }

        lines.push(Line::from(spans));
    }

    // 底部状态行（V13: 含选中位置）
    let visible_count = content_rows * cols_per_row;
    let status_text = if is_focused {
        format!(" {} / {} 选中 · Enter 填充输入 ", sel + 1, all_commands.len())
    } else if all_commands.len() > visible_count {
        format!(" ↑↓滚动 · {}/{} ", scroll + 1, max_scroll + 1)
    } else {
        format!(" 共 {} 条 ｜ 全部可见 ", all_commands.len())
    };
    lines.push(Line::from(vec![
        Span::styled(status_text, state.theme.text_style(TextRole::Caption)),
    ]));

    f.render_widget(Paragraph::new(lines), inner);
}
